import { awaitWithAbort } from "../../async-work/cancellation/index.ts";
import {
	createResourceScope,
	type ResourceScope,
} from "../../resource-scopes/index.ts";
import {
	callWithResult,
	callWithResultAsync,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../../results/index.ts";
import type { SseEvent } from "./event.ts";
import { createSseParser, type SseParser } from "./parser.ts";

/** Observable connection state; no network activity starts until iteration. */
export type SseConnectionState =
	| "idle"
	| "connecting"
	| "open"
	| "reconnecting"
	| "closed";
/** Explicit transport bounds and reconnect policy for a standard GET stream. */
export type SseClientOptions = {
	readonly url: string | URL;
	/**
	 * Native Fetch options, excluding method, body, signal, and cache. Every
	 * request uses the `no-store` cache mode. The client sets `Accept` and
	 * `Last-Event-ID`, so headers that contain either name are invalid options.
	 */
	readonly request?: Omit<
		RequestInit,
		"method" | "body" | "signal" | "cache"
	>;
	readonly signal?: AbortSignal;
	readonly maxFrameSize: number;
	/** Open and pending-read deadlines; null disables the respective deadline. */
	readonly openTimeoutMillis: number | null;
	readonly idleTimeoutMillis: number | null;
	/** Specifies the total reconnect budget across the lifetime of this connection; each reconnect attempt consumes one attempt from this budget even after an intervening successful connection. */
	readonly reconnect: {
		readonly maxAttempts: number;
		readonly delayMillis: number;
		readonly maxDelayMillis: number;
	};
};
/** Invalid limits, retry settings, timeouts, URL, native Request options, or
 * request headers that contain `Accept` or `Last-Event-ID`. */
export type SseOptionsError = {
	readonly kind: "invalid-options";
	readonly cause?: unknown;
};
/** Connection failures are terminal except `concurrent-read`, which rejects only
 * that call. Transient transport failures consume the reconnect budget. */
export type SseClientError =
	| { readonly kind: "http-status"; readonly status: number }
	| { readonly kind: "content-type"; readonly value: string | null }
	| { readonly kind: "missing-body" | "frame-too-large" | "concurrent-read" }
	| { readonly kind: "transport"; readonly cause: unknown }
	| { readonly kind: "timeout"; readonly phase: "open" | "read" }
	| {
			readonly kind: "reconnect-exhausted";
			readonly cause: SseClientError | null;
	  };
/** A single-consumer iterator. Closing or returning cancels reads and reconnect waits. */
export type SseConnection = AsyncIterableIterator<
	Result<SseEvent, SseClientError>
> & {
	/** Resolves with the local cleanup result when the connection closes. */
	readonly closed: Promise<Result<void, unknown>>;
	/**
	 * Closes the connection and runs its cleanup once. If a cleanup function
	 * throws, such as a stream reader's `releaseLock`, the remaining cleanup
	 * still runs and `close` then throws an AggregateError of the failures.
	 * `return` and `throw` close the connection in the same way, so their
	 * promises reject in that case. The `closed` promise retains the cleanup
	 * result, including when a read or an abort closes the connection without
	 * replacing that operation's result.
	 */
	close(): void;
	getState(): SseConnectionState;
};

const MAX_TIMER_MILLIS = 2_147_483_647;
function checkDelay(value: number): boolean {
	return (
		Number.isSafeInteger(value) && value >= 0 && value <= MAX_TIMER_MILLIS
	);
}

function clampRetryDelay(
	value: bigint,
	options: SseClientOptions["reconnect"],
): number {
	if (value > BigInt(options.maxDelayMillis)) {
		return options.maxDelayMillis;
	}
	if (value < BigInt(options.delayMillis)) {
		return options.delayMillis;
	}
	return Number(value);
}

/** Creates a connection without starting I/O. Iteration starts Fetch; each event
 * suspends further parsing and reads until the consumer asks for the next event.
 * Returning from iteration aborts the active Fetch even during a stalled read.
 * HTTP 204 ends cleanly. Other non-200 statuses, invalid MIME types, and parser
 * failures are terminal. Retry fields are clamped to the configured delay range.
 */
export function createSseConnection(
	input: SseClientOptions,
): Result<SseConnection, SseOptionsError> {
	if (
		!Number.isSafeInteger(input.maxFrameSize) ||
		input.maxFrameSize < 1 ||
		!Number.isSafeInteger(input.reconnect.maxAttempts) ||
		input.reconnect.maxAttempts < 0 ||
		!checkDelay(input.reconnect.delayMillis) ||
		!checkDelay(input.reconnect.maxDelayMillis) ||
		input.reconnect.maxDelayMillis < input.reconnect.delayMillis ||
		[input.openTimeoutMillis, input.idleTimeoutMillis].some(
			(value) => value !== null && (!checkDelay(value) || value === 0),
		)
	) {
		return createErrorResult({ kind: "invalid-options" });
	}
	const initial = callWithResult(() => {
		return new Request(input.url, {
			...input.request,
			method: "GET",
			cache: "no-store",
		});
	});
	if (initial.kind === "error") {
		return createErrorResult({
			kind: "invalid-options",
			cause: initial.error,
		});
	}
	const request = initial.value;
	if (request.headers.has("accept") || request.headers.has("last-event-id")) {
		return createErrorResult({ kind: "invalid-options" });
	}
	const externalSignal = input.signal;
	// Snapshot caller configuration before an asynchronous lifecycle begins.
	const options = {
		maxFrameSize: input.maxFrameSize,
		openTimeoutMillis: input.openTimeoutMillis,
		idleTimeoutMillis: input.idleTimeoutMillis,
		reconnect: { ...input.reconnect },
	};
	type Attempt = {
		readonly scope: ResourceScope;
		/** A deadline or the scope's cleanup aborts this controller. */
		readonly transport: AbortController;
	};
	type BufferedInput =
		| { readonly kind: "awaiting-chunk" }
		| {
				readonly kind: "buffered";
				readonly chunk: Uint8Array;
				readonly offset: number;
		  };
	type ConnectionState =
		| { readonly kind: "idle" }
		| {
				readonly kind: "opening";
				readonly mode: "initial" | "retry";
				readonly attempt: Attempt;
		  }
		| {
				readonly kind: "open";
				readonly attempt: Attempt;
				readonly reader: ReadableStreamDefaultReader<Uint8Array>;
				readonly parser: SseParser;
				buffered: BufferedInput;
		  }
		| {
				readonly kind: "reconnect-wait";
				readonly scope: ResourceScope;
				readonly ready: Promise<void>;
		  }
		| { readonly kind: "closed" };
	type ClientRead = IteratorResult<Result<SseEvent, SseClientError>, void>;
	let completeClose!: (result: Result<void, unknown>) => void;
	const closed = new Promise<Result<void, unknown>>((resolve) => {
		completeClose = resolve;
	});
	const lifetime = createResourceScope({
		onCleanupErrorFn: (cause) => {
			throw cause;
		},
	});
	let state: ConnectionState = { kind: "idle" };
	let admission:
		| { readonly kind: "available" }
		| { readonly kind: "reading"; readonly completed: Promise<void> } = {
		kind: "available",
	};
	let lastEventId = "";
	let retryMillis = options.reconnect.delayMillis;
	let reconnects = 0;

	function getScope(phase: ConnectionState): ResourceScope | null {
		switch (phase.kind) {
			case "opening":
			case "open":
				return phase.attempt.scope;
			case "reconnect-wait":
				return phase.scope;
			case "idle":
			case "closed":
				return null;
		}
	}

	function transition(next: ConnectionState): void {
		if (state.kind === "closed") {
			getScope(next)?.dispose();
			return;
		}
		const previous = state;
		state = next;
		const previousScope = getScope(previous);
		if (previousScope !== null && previousScope !== getScope(next)) {
			previousScope.dispose();
		}
	}

	function close(): void {
		if (state.kind === "closed") {
			return;
		}
		state = { kind: "closed" };
		const cleanup = callWithResult(() => lifetime.dispose());
		completeClose(cleanup);
		if (cleanup.kind === "error") {
			throw cleanup.error;
		}
	}
	lifetime.addCleanup(() => {
		externalSignal?.removeEventListener("abort", closeForRead);
	});
	if (externalSignal?.aborted) {
		closeForRead();
	} else {
		externalSignal?.addEventListener("abort", closeForRead, { once: true });
	}

	function startAttempt(mode: "initial" | "retry"): void {
		const scope = lifetime.createChild();
		const transport = new AbortController();
		scope.addCleanup(() => transport.abort());
		transition({ kind: "opening", mode, attempt: { scope, transport } });
	}

	function createDeadline(
		attempt: Attempt,
		delay: {
			readonly millis: number | null;
			readonly phase: "open" | "read";
		},
	): () => SseClientError | null {
		if (delay.millis === null) {
			return () => null;
		}
		let outcome: "pending" | "expired" = "pending";
		const timer = setTimeout(() => {
			outcome = "expired";
			attempt.transport.abort();
		}, delay.millis);
		const detach = attempt.scope.addCleanup(() => clearTimeout(timer));
		return () => {
			clearTimeout(timer);
			detach();
			return outcome === "expired"
				? { kind: "timeout", phase: delay.phase }
				: null;
		};
	}

	/** Closes without replacing a read or abort result. `closed` retains cleanup failures. */
	function closeForRead(): void {
		callWithResult(close);
	}

	function fail(error: SseClientError): ClientRead {
		closeForRead();
		return { done: false, value: createErrorResult(error) };
	}

	function scheduleReconnect(
		cause: SseClientError | null,
	): ClientRead | null {
		if (reconnects >= options.reconnect.maxAttempts) {
			return fail({ kind: "reconnect-exhausted", cause });
		}
		reconnects += 1;
		const scope = lifetime.createChild();
		const ready = new Promise<void>((resolve) => {
			const timer = setTimeout(resolve, retryMillis);
			scope.addCleanup(() => clearTimeout(timer));
		});
		transition({ kind: "reconnect-wait", scope, ready });
		return null;
	}

	async function readNext(): Promise<ClientRead> {
		while (true) {
			const current = state;
			switch (current.kind) {
				case "closed":
					return { done: true, value: undefined };
				case "idle":
					startAttempt("initial");
					break;
				case "reconnect-wait": {
					await awaitWithAbort({
						signal: current.scope.signal,
						operationFn: () => current.ready,
					});
					if (state !== current) {
						return { done: true, value: undefined };
					}
					startAttempt("retry");
					break;
				}
				case "opening": {
					const headers = new Headers(request.headers);
					headers.set("accept", "text/event-stream");
					if (lastEventId !== "") {
						let header = "";
						for (const byte of new TextEncoder().encode(
							lastEventId,
						)) {
							header += String.fromCharCode(byte);
						}
						headers.set("last-event-id", header);
					}
					const finishDeadline = createDeadline(current.attempt, {
						millis: options.openTimeoutMillis,
						phase: "open",
					});
					const opened = await awaitWithAbort({
						signal: current.attempt.transport.signal,
						operationFn: () => {
							return fetch(
								new Request(request, {
									headers,
									signal: current.attempt.transport.signal,
								}),
							);
						},
					});
					const timeout = finishDeadline();
					if (state !== current) {
						return { done: true, value: undefined };
					}
					if (opened.kind === "error") {
						const terminal = scheduleReconnect(
							timeout ?? {
								kind: "transport",
								cause: opened.error,
							},
						);
						if (terminal !== null) {
							return terminal;
						}
						break;
					}
					const response = opened.value;
					if (response.status === 204) {
						closeForRead();
						return { done: true, value: undefined };
					}
					const body = parseEventStreamBody(response);
					if (body.kind === "error") {
						const terminal = fail(body.error);
						void callWithResultAsync(() => response.body?.cancel());
						return terminal;
					}
					const parser = createSseParser({
						maxFrameSize: options.maxFrameSize,
						initialEventId: lastEventId,
					});
					if (parser.kind === "error") {
						throw new Error(
							"Retained SSE parser configuration became invalid",
						);
					}
					const reader = body.value.getReader();
					current.attempt.scope.addCleanup(() => {
						void callWithResultAsync(() => reader.cancel());
						reader.releaseLock();
					});
					transition({
						kind: "open",
						attempt: current.attempt,
						reader,
						parser: parser.value,
						buffered: { kind: "awaiting-chunk" },
					});
					break;
				}
				case "open": {
					if (current.buffered.kind === "buffered") {
						const buffered = current.buffered;
						const parsed = current.parser.readFrame({
							chunk: buffered.chunk,
							offset: buffered.offset,
						});
						if (parsed.kind === "error") {
							return fail({ kind: "frame-too-large" });
						}
						current.buffered =
							parsed.value.offset === buffered.chunk.length
								? { kind: "awaiting-chunk" }
								: {
										kind: "buffered",
										chunk: buffered.chunk,
										offset: parsed.value.offset,
									};
						const serverRetry = current.parser.getRetryMillis();
						if (serverRetry !== null) {
							retryMillis = clampRetryDelay(
								serverRetry,
								options.reconnect,
							);
						}
						if (parsed.value.frame !== null) {
							lastEventId = current.parser.getLastEventId();
							if (parsed.value.frame.event !== null) {
								return {
									done: false,
									value: createSuccessResult(
										parsed.value.frame.event,
									),
								};
							}
						}
						break;
					}
					const finishDeadline = createDeadline(current.attempt, {
						millis: options.idleTimeoutMillis,
						phase: "read",
					});
					const read = await awaitWithAbort({
						signal: current.attempt.transport.signal,
						operationFn: () => current.reader.read(),
					});
					const timeout = finishDeadline();
					if (state !== current) {
						return { done: true, value: undefined };
					}
					if (read.kind === "error" || read.value.done) {
						const terminal = scheduleReconnect(
							read.kind === "error"
								? (timeout ?? {
										kind: "transport",
										cause: read.error,
									})
								: null,
						);
						if (terminal !== null) {
							return terminal;
						}
						break;
					}
					if (read.value.value.length > 0) {
						current.buffered = {
							kind: "buffered",
							chunk: read.value.value,
							offset: 0,
						};
					}
					break;
				}
			}
		}
	}

	const connection: SseConnection = {
		closed,
		[Symbol.asyncIterator]() {
			return connection;
		},
		async next() {
			if (admission.kind === "reading") {
				return {
					done: false,
					value: createErrorResult({ kind: "concurrent-read" }),
				};
			}
			let completeRead!: () => void;
			admission = {
				kind: "reading",
				completed: new Promise<void>((resolve) => {
					completeRead = resolve;
				}),
			};
			const result = await callWithResultAsync(readNext);
			const outcome =
				result.kind === "error"
					? fail({ kind: "transport", cause: result.error })
					: result.value;
			admission = { kind: "available" };
			completeRead();
			return outcome;
		},
		async return() {
			const pending =
				admission.kind === "reading" ? admission.completed : null;
			close();
			await pending;
			return { done: true, value: undefined };
		},
		async throw(cause?: unknown) {
			const pending =
				admission.kind === "reading" ? admission.completed : null;
			close();
			await pending;
			return {
				done: false,
				value: createErrorResult({ kind: "transport", cause }),
			};
		},
		close,
		getState: () => {
			switch (state.kind) {
				case "idle":
					return "idle";
				case "opening":
					return state.mode === "initial"
						? "connecting"
						: "reconnecting";
				case "open":
					return "open";
				case "reconnect-wait":
					return "reconnecting";
				case "closed":
					return "closed";
			}
		},
	};
	return createSuccessResult(connection);
}

function parseEventStreamBody(
	response: Response,
): Result<ReadableStream<Uint8Array>, SseClientError> {
	if (response.status !== 200) {
		return createErrorResult({
			kind: "http-status",
			status: response.status,
		});
	}
	const contentType = response.headers.get("content-type");
	if (
		contentType?.split(";", 1)[0]?.trim().toLowerCase() !==
		"text/event-stream"
	) {
		return createErrorResult({ kind: "content-type", value: contentType });
	}
	return response.body === null
		? createErrorResult({ kind: "missing-body" })
		: createSuccessResult(response.body);
}
