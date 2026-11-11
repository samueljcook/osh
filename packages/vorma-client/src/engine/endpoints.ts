import { awaitWithAbort } from "primitives-ts/async-work/cancellation";
import { getErrorMsg } from "primitives-ts/error-msgs";
import { fetchWithResult } from "primitives-ts/http/fetch";
import { createFormData, type FormFields } from "primitives-ts/http/form-data";
import { type ResourceScope } from "primitives-ts/resource-scopes";
import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "primitives-ts/results";
import { createRandomId } from "../foundation/random-id.ts";
import type { RawApiEndpointCall } from "../generated-types.ts";
import { HTTP_PROTOCOL } from "../protocol/constants.ts";
import { buildHref, type HrefSearch } from "../protocol/href.ts";
import {
	encodeSearchKeySegment,
	escapeSearchScalarText,
} from "../protocol/search.ts";
import {
	readEndpointFailureEnvelope,
	readRedirectStatus,
} from "../protocol/wire.ts";
import type {
	ApiEndpointCallId,
	ApiEndpointCallResult,
	MutationResult,
	NavResult,
	RevalidationResult,
} from "../results.ts";
import { awaitBootMetadata, getBootMetadata } from "./boot-metadata.ts";
import { reportBuildSkew } from "./build-skew.ts";
import { awaitBoot } from "./client.ts";
import type { ClientCtx, EndpointInterruption, EndpointRun } from "./ctx.ts";
import { publishEvent } from "./events.ts";
import { startNav, type NavInput } from "./nav.ts";
import { retirePreparationsAndOutputs } from "./prefetch.ts";
import { registerRevalidationDemand } from "./revalidation.ts";
import { updateState } from "./state.ts";
import { checkNavigableOrigin } from "./targets.ts";

/*
Endpoint calls. A call has a kind, a method, and a pattern; the generated types
reject any combination that the server does not declare. A mutation owes a
revalidation from the moment it is dispatched, unless the call opts out. The
call registers the demand when it settles: with a response, with a failure,
with a cancel by the caller, or with a replacement by a later call with the
same supersede key. A response that never arrives registers nothing until the
caller cancels the call or a later call replaces it. A call made before the
first commit runs as soon as boot has read the document's build ID because a
client loader that prepares the first page may await it and boot reads the
build ID before it runs any loader; no revalidation demand exists before the
first commit, so such a call owes none, and a redirect that it returns starts
its navigation at the first commit.
*/

const REQUEST_HEADERS = HTTP_PROTOCOL.requestHeaders;
const RESPONSE_HEADERS = HTTP_PROTOCOL.responseHeaders;

/**
 * Writes a form's entries as the query of a GET or HEAD call. Each name is
 * spelled as one key segment of the search grammar (`protocol/search.ts`), and
 * each nonempty value as a string scalar. An empty value stays empty, which
 * the grammar reads as a field that nothing was entered into, as a browser
 * submits an empty input. A file entry is an error because a query cannot
 * carry it.
 */
function writeFormQuery(input: {
	readonly form: FormData;
	readonly method: string;
}): Result<string, string> {
	const params = new URLSearchParams();
	for (const [name, value] of input.form.entries()) {
		if (typeof value !== "string") {
			return createErrorResult(
				`The form field ${JSON.stringify(name)} holds a file, which a ${input.method} request cannot carry.`,
			);
		}
		params.append(
			encodeSearchKeySegment(name),
			escapeSearchScalarText(value),
		);
	}
	return createSuccessResult(params.toString());
}

type BuiltRequest = {
	readonly request: Request;
	readonly href: string;
	/** A text that identifies the request's body, for sharing; undefined when the body cannot be compared. */
	readonly bodyKey: string | undefined;
};

/** Builds the request for a call, or says why its input cannot be sent. */
export function buildEndpointRequest(
	ctx: ClientCtx,
	call: RawApiEndpointCall,
): Result<BuiltRequest, string> {
	const metadata = getBootMetadata(ctx);
	const method = call.method.toUpperCase();
	const shouldCarryInputInSearch = method === "GET" || method === "HEAD";
	let search: HrefSearch = { kind: "none" };
	let body: BodyInit | undefined;
	let bodyKey: string | undefined;
	let jsonBody = false;
	const { formInput } = call;
	if (
		formInput !== undefined &&
		(typeof formInput !== "object" || formInput === null)
	) {
		return createErrorResult(
			"The form input must be an object of the form's fields or a FormData.",
		);
	}
	let input = formInput === undefined ? call.input : formInput;
	if (formInput !== undefined && !(formInput instanceof FormData)) {
		// The generated call types give the application the form's field types,
		// and createFormData checks every field value at runtime.
		const converted = createFormData(formInput as FormFields);
		if (converted.kind === "error") {
			return createErrorResult(
				"The form input contains a value that cannot be sent as a form field.",
			);
		}
		input = converted.value;
	}
	if (input instanceof FormData) {
		if (shouldCarryInputInSearch) {
			const query = writeFormQuery({ form: input, method });
			if (query.kind === "error") {
				return query;
			}
			search = { kind: "query-text", text: query.value };
		} else {
			body = input;
		}
	} else if (shouldCarryInputInSearch) {
		search = { kind: "input", input };
	} else if (input !== undefined) {
		const text = callWithResult(() => JSON.stringify(input));
		if (text.kind === "error" || typeof text.value !== "string") {
			return createErrorResult("The input cannot be written as JSON.");
		}
		body = text.value;
		bodyKey = text.value;
		jsonBody = true;
	}
	const href = buildHref({
		kind: "endpoint",
		pattern: call.pattern,
		params: call.params,
		splatValues: call.splatValues,
		search,
		skewProtectionEnabled: metadata.deploymentId !== undefined,
	});
	if (href.kind === "error") {
		return createErrorResult(href.error.msg);
	}
	const url = new URL(href.value, window.location.href);
	const { headers: headerInit, ...rest } = call.requestOptions ?? {};
	const parsedHeaders = callWithResult(() => new Headers(headerInit));
	if (parsedHeaders.kind === "error") {
		return createErrorResult(
			getErrorMsg(parsedHeaders.error) ??
				"The request headers cannot be built.",
		);
	}
	const headers = parsedHeaders.value;
	if (body instanceof FormData && headers.has("content-type")) {
		return createErrorResult(
			"A call with form input must not set the content-type header because the browser writes that header with the boundary of the form's parts.",
		);
	}
	headers.set("accept", "application/json");
	headers.set(REQUEST_HEADERS.clientBuildId, metadata.clientBuildId);
	headers.set(REQUEST_HEADERS.clientAcceptsRedirect, "1");
	if (metadata.deploymentId !== undefined) {
		headers.set(REQUEST_HEADERS.vercelDeploymentId, metadata.deploymentId);
	}
	if (jsonBody && !headers.has("content-type")) {
		headers.set("content-type", "application/json");
	}
	const built = callWithResult(() => {
		return new Request(url, {
			...rest,
			method,
			headers,
			...(body === undefined ? {} : { body }),
		});
	});
	if (built.kind === "error") {
		return createErrorResult(
			getErrorMsg(built.error) ?? "The request cannot be built.",
		);
	}
	return createSuccessResult({
		request: built.value,
		href: url.href,
		bodyKey,
	});
}

function setCallState(
	ctx: ClientCtx,
	callId: ApiEndpointCallId,
	change: { readonly href?: string; readonly phase?: "preparing" | "sent" },
): void {
	updateState(ctx, (state) => {
		const apiEndpointCalls = state.apiEndpointCalls.map((entry) => {
			return entry.callId === callId
				? {
						...entry,
						href: change.href ?? entry.href,
						phase: change.phase ?? entry.phase,
					}
				: entry;
		});
		return { ...state, apiEndpointCalls };
	});
}

function removeCallState(ctx: ClientCtx, callId: ApiEndpointCallId): void {
	updateState(ctx, (state) => {
		const apiEndpointCalls = state.apiEndpointCalls.filter((entry) => {
			return entry.callId !== callId;
		});
		return { ...state, apiEndpointCalls };
	});
}

/**
 * Sends a call and resolves with its result. Valid calls never reject. A query
 * call sent with `endsWithGeneration` is interrupted when the session's
 * generation ends, and resolves as aborted.
 */
export function callApiEndpointRaw(
	ctx: ClientCtx,
	call: RawApiEndpointCall,
	options: {
		readonly endsWithGeneration: boolean;
		/** A watched query uses the same request for comparison and dispatch. */
		readonly builtRequest?: Result<BuiltRequest, string>;
	},
): Promise<ApiEndpointCallResult | MutationResult> {
	const lifecycleAtCall = ctx.state.getValue().lifecycle;
	if (
		lifecycleAtCall !== "ready" &&
		lifecycleAtCall !== "pre-boot" &&
		lifecycleAtCall !== "booting"
	) {
		return Promise.resolve({ kind: "client-disposed" });
	}
	const callId = createRandomId() as ApiEndpointCallId;
	if (call.signal?.aborted === true) {
		return Promise.resolve({ kind: "aborted", callId });
	}
	const scope = ctx.scope.createChild();
	type CallState =
		| { readonly kind: "preparing" }
		| { readonly kind: "dispatched" }
		| {
				readonly kind: "interrupted";
				readonly cause: EndpointInterruption;
				readonly dispatched: boolean;
		  }
		| {
				readonly kind: "settled";
				readonly result: ApiEndpointCallResult | MutationResult;
		  };
	let callState: CallState = { kind: "preparing" };
	/** Ends the call; the first cause decides its result. */
	function interrupt(cause: EndpointInterruption): void {
		if (callState.kind === "settled" || callState.kind === "interrupted") {
			return;
		}
		callState = {
			kind: "interrupted",
			cause,
			dispatched: callState.kind === "dispatched",
		};
		scope.dispose();
	}
	const run: EndpointRun = {
		interrupt,
		endsWithGeneration: options.endsWithGeneration && call.kind === "query",
	};
	function handleCallerAbort(): void {
		interrupt("caller-aborted");
	}
	call.signal?.addEventListener("abort", handleCallerAbort, { once: true });
	scope.addCleanup(() =>
		call.signal?.removeEventListener("abort", handleCallerAbort),
	);
	if (call.supersedeKey !== undefined) {
		ctx.runsBySupersedeKey.get(call.supersedeKey)?.interrupt("superseded");
		ctx.runsBySupersedeKey.set(call.supersedeKey, run);
	}
	ctx.endpointRuns.add(run);
	const shouldRegisterRevalidationDemand =
		lifecycleAtCall === "ready" &&
		call.kind === "mutation" &&
		call.skipAutoRevalidate !== true;
	updateState(ctx, (state) => {
		const started = {
			callId,
			kind: call.kind,
			method: call.method,
			pattern: call.pattern,
			href: undefined,
			phase: "preparing" as const,
			skipWorkIndicator: call.skipWorkIndicator === true,
		};
		return {
			...state,
			apiEndpointCalls: [...state.apiEndpointCalls, started],
		};
	});

	let revalidation:
		| { readonly kind: "unregistered" }
		| {
				readonly kind: "registered";
				readonly promise: Promise<RevalidationResult>;
		  } = { kind: "unregistered" };

	/**
	 * Registers the call's revalidation demand once, and returns the promise of
	 * its result. `afterSequence` is the sequence that a satisfying read must
	 * follow, when it is not the next one.
	 */
	function registerCallRevalidationDemand(
		afterSequence?: number,
	): Promise<RevalidationResult> {
		if (revalidation.kind === "registered") {
			return revalidation.promise;
		}
		const completion = Promise.withResolvers<RevalidationResult>();
		revalidation = { kind: "registered", promise: completion.promise };
		registerRevalidationDemand(ctx, {
			immediate: true,
			skipWorkIndicator: call.skipWorkIndicator === true,
			waiter: completion.resolve,
			...(afterSequence === undefined ? {} : { afterSequence }),
		});
		return completion.promise;
	}

	function settle(
		result: ApiEndpointCallResult,
	): ApiEndpointCallResult | MutationResult {
		const previous = callState;
		if (previous.kind === "settled") {
			return previous.result;
		}
		const wasDispatched =
			previous.kind === "dispatched" ||
			(previous.kind === "interrupted" && previous.dispatched);
		callState = { kind: "settled", result };
		call.signal?.removeEventListener("abort", handleCallerAbort);
		ctx.endpointRuns.delete(run);
		if (
			call.supersedeKey !== undefined &&
			ctx.runsBySupersedeKey.get(call.supersedeKey) === run
		) {
			ctx.runsBySupersedeKey.delete(call.supersedeKey);
		}
		let settled: ApiEndpointCallResult | MutationResult = result;
		if (
			wasDispatched &&
			shouldRegisterRevalidationDemand &&
			result.kind !== "client-disposed" &&
			ctx.state.getValue().lifecycle === "ready"
		) {
			// The demand registers before the call leaves the state, so the work
			// indicator sees no idle moment between a mutation and its
			// revalidation. A redirect that leaves the application reads nothing
			// more because the document is leaving.
			const isLeavingApplication =
				result.kind === "redirect" &&
				!checkNavigableOrigin(new URL(result.href));
			const revalidation = isLeavingApplication
				? Promise.resolve<RevalidationResult>({ kind: "cancelled" })
				: registerCallRevalidationDemand();
			settled = { ...result, revalidation };
		}
		removeCallState(ctx, callId);
		callState = { kind: "settled", result: settled };
		scope.dispose();
		publishEvent(ctx, {
			kind: "api-endpoint-call-settled",
			callId,
			result: settled,
		});
		return settled;
	}

	async function runCallPipeline(): Promise<ApiEndpointCallResult> {
		if (lifecycleAtCall === "pre-boot" || lifecycleAtCall === "booting") {
			// Every request names the build that the document carries, so a call
			// made before boot waits until boot has read it.
			const metadataRead = await awaitWithAbort({
				signal: scope.signal,
				operationFn: () => awaitBootMetadata(ctx),
			});
			if (metadataRead.kind === "error") {
				return (
					getInterruptedResult() ??
					(metadataRead.error.kind === "aborted"
						? { kind: "aborted", callId }
						: toTransportFailureResult(metadataRead.error.cause))
				);
			}
			if (!metadataRead.value) {
				return { kind: "client-disposed" };
			}
		}
		const built = options.builtRequest ?? buildEndpointRequest(ctx, call);
		if (built.kind === "error") {
			return { kind: "invalid-input", msg: built.error };
		}
		setCallState(ctx, callId, { href: built.value.href });
		let request = built.value.request;
		const prepare = ctx.options.prepareRequestFn;
		if (prepare !== undefined) {
			// A preparation that never settles never delays the end of the call.
			const prepared = await awaitWithAbort({
				signal: scope.signal,
				operationFn: () =>
					prepare({ request, requestKind: call.kind, callId }),
			});
			if (prepared.kind === "error") {
				return (
					getInterruptedResult() ??
					(prepared.error.kind === "aborted"
						? { kind: "aborted", callId }
						: toTransportFailureResult(prepared.error.cause))
				);
			}
			request = prepared.value;
		}
		if (scope.signal.aborted || callState.kind !== "preparing") {
			return getInterruptedResult() ?? { kind: "aborted", callId };
		}
		callState = { kind: "dispatched" };
		setCallState(ctx, callId, { phase: "sent" });
		// A fetch that ignores its signal never delays the end of the call.
		const sent = await fetchWithResult({
			request,
			options: { signal: scope.signal },
		});
		if (sent.kind === "error") {
			return (
				getInterruptedResult() ??
				(sent.error.kind === "aborted"
					? { kind: "aborted", callId }
					: toTransportFailureResult(sent.error.cause))
			);
		}
		return classifyResponse(ctx, {
			response: sent.value,
			call,
			callId,
			requestHref: built.value.href,
			scope,
			getInterruptedResultFn: getInterruptedResult,
			registerCallRevalidationDemandFn: shouldRegisterRevalidationDemand
				? registerCallRevalidationDemand
				: undefined,
		});
	}

	/** The result of a call that something ended before it settled, or undefined when nothing did. */
	function getInterruptedResult(): ApiEndpointCallResult | undefined {
		if (callState.kind !== "interrupted") {
			return undefined;
		}
		switch (callState.cause) {
			case "disposed":
				return { kind: "client-disposed" };
			case "superseded":
				return { kind: "superseded", callId };
			case "caller-aborted":
			case "generation-retired":
				return { kind: "aborted", callId };
		}
	}

	function toTransportFailureResult(cause: unknown): ApiEndpointCallResult {
		return {
			kind: "transport-failure",
			callId,
			msg: getErrorMsg(cause) ?? "The request failed.",
			cause,
		};
	}

	return runCallPipeline().then(settle);
}

async function classifyResponse(
	ctx: ClientCtx,
	input: {
		readonly response: Response;
		readonly call: RawApiEndpointCall;
		readonly callId: ApiEndpointCallId;
		readonly requestHref: string;
		readonly scope: ResourceScope;
		readonly getInterruptedResultFn: () =>
			| ApiEndpointCallResult
			| undefined;
		/** Registers the call's revalidation demand; absent when the call owes none. */
		readonly registerCallRevalidationDemandFn:
			| ((afterSequence?: number) => unknown)
			| undefined;
	},
): Promise<ApiEndpointCallResult> {
	const { response, call, callId } = input;
	// A response always names its URL in a browser.
	const responseUrl = response.url === "" ? input.requestHref : response.url;
	if (input.scope.signal.aborted) {
		// A late response for a call that something closed changes nothing.
		return input.getInterruptedResultFn() ?? { kind: "aborted", callId };
	}
	const serverBuildId = response.headers.get(RESPONSE_HEADERS.clientBuildId);
	const responseMeta = { status: response.status, headers: response.headers };
	if (response.status === 304) {
		// Fetch exposes a 304 only for a conditional request that application
		// code made itself.
		return {
			kind: "transport-failure",
			callId,
			msg: "The endpoint answered 304 Not Modified to a conditional request header, such as If-None-Match, that application code set. Endpoint calls do not support conditional request headers.",
			cause: new Error("The endpoint answered 304 Not Modified."),
		};
	}
	if (
		response.redirected &&
		(serverBuildId === null ||
			(response.headers.get("content-type") ?? "").includes("text/html"))
	) {
		return startRedirect(ctx, {
			callId,
			href: responseUrl,
			redirectStatus: undefined,
			call,
			registerCallRevalidationDemandFn:
				input.registerCallRevalidationDemandFn,
		});
	}
	if (serverBuildId === null) {
		return {
			kind: "transport-failure",
			callId,
			msg: "The response is not Vorma's.",
			cause: new Error("The response names no Vorma build."),
		};
	}
	if (serverBuildId !== getBootMetadata(ctx).clientBuildId) {
		const requestedUrl = new URL(input.requestHref);
		reportBuildSkew(ctx, {
			clientBuildId: getBootMetadata(ctx).clientBuildId,
			serverBuildId,
			requestedHref: `${requestedUrl.pathname}${requestedUrl.search}`,
			requestKind: call.kind,
			status: response.status,
		});
	}
	const redirectTarget = response.headers.get(
		RESPONSE_HEADERS.clientRedirect,
	);
	if (redirectTarget !== null) {
		const status = readRedirectStatus(
			response.headers.get(RESPONSE_HEADERS.clientRedirectStatus),
		);
		const target = callWithResult(
			() => new URL(redirectTarget, responseUrl),
		);
		if (
			status.kind === "error" ||
			target.kind === "error" ||
			(target.value.protocol !== "http:" &&
				target.value.protocol !== "https:")
		) {
			return {
				kind: "transport-failure",
				callId,
				msg: "The redirect response is malformed.",
				cause: status.kind === "error" ? status.error : target,
			};
		}
		return startRedirect(ctx, {
			callId,
			href: target.value.href,
			redirectStatus: status.value,
			call,
			registerCallRevalidationDemandFn:
				input.registerCallRevalidationDemandFn,
		});
	}
	const text = await awaitWithAbort({
		signal: input.scope.signal,
		operationFn: () => response.text(),
	});
	if (text.kind === "error") {
		if (text.error.kind === "aborted") {
			return (
				input.getInterruptedResultFn() ?? { kind: "aborted", callId }
			);
		}
		return (
			input.getInterruptedResultFn() ?? {
				kind: "transport-failure",
				callId,
				msg: "The response body could not be read.",
				cause: text.error.cause,
			}
		);
	}
	if (input.scope.signal.aborted) {
		// The answer arrived after the call ended; it never reaches the caller.
		return input.getInterruptedResultFn() ?? { kind: "aborted", callId };
	}
	if (response.status < 200 || response.status >= 300) {
		const parsed = callWithResult(() => JSON.parse(text.value) as unknown);
		const envelope =
			parsed.kind === "ok"
				? readEndpointFailureEnvelope(parsed.value)
				: parsed;
		if (
			envelope.kind === "error" ||
			envelope.value.status !== response.status
		) {
			return {
				kind: "transport-failure",
				callId,
				msg: "The failure response does not match its status.",
				cause:
					envelope.kind === "error" ? envelope.error : envelope.value,
			};
		}
		const failure = envelope.value;
		switch (failure.kind) {
			case "declared-error":
				return {
					kind: "declared-error",
					callId,
					declaredError: failure.declaredError,
					...responseMeta,
				};
			case "rejection":
				return {
					kind: "rejection",
					callId,
					clientMsg: failure.clientMsg,
					...responseMeta,
				};
			case "undeclared-failure":
				return { kind: "undeclared-failure", callId, ...responseMeta };
		}
	}
	if (response.headers.get(RESPONSE_HEADERS.noOutput) !== null) {
		return text.value === ""
			? { kind: "success", callId, output: undefined, ...responseMeta }
			: {
					kind: "transport-failure",
					callId,
					msg: "A response without output has a body.",
					cause: new Error(
						"The response has a body and the no-output header.",
					),
				};
	}
	const output = callWithResult(() => JSON.parse(text.value) as unknown);
	return output.kind === "ok"
		? { kind: "success", callId, output: output.value, ...responseMeta }
		: {
				kind: "transport-failure",
				callId,
				msg: "The response body is not JSON.",
				cause: output.error,
			};
}

function startRedirect(
	ctx: ClientCtx,
	input: {
		readonly callId: ApiEndpointCallId;
		readonly href: string;
		readonly redirectStatus: 302 | 303 | 307 | 308 | undefined;
		readonly call: RawApiEndpointCall;
		readonly registerCallRevalidationDemandFn:
			| ((afterSequence?: number) => unknown)
			| undefined;
	},
): ApiEndpointCallResult {
	const sequenceBeforeRead = ctx.lastSequence;
	// A redirect that leaves the application owes no read.
	const owesRevalidation =
		input.registerCallRevalidationDemandFn !== undefined &&
		checkNavigableOrigin(new URL(input.href));
	const navInput: NavInput = {
		href: input.href,
		replace: true,
		historyState: undefined,
		scrollToTop: undefined,
		scrollToHash: undefined,
		skipWorkIndicator: input.call.skipWorkIndicator === true,
		source: "redirect",
		read: owesRevalidation ? "own" : "shared",
	};
	if (owesRevalidation) {
		// The navigation must not commit a page that was read before the
		// mutation: the prefetched and retained pages retire before it starts,
		// and it reads its destination itself instead of joining or taking the
		// read of a pending navigation.
		retirePreparationsAndOutputs(ctx);
	}
	// Before the first commit there is no page to leave, so the navigation
	// starts when the first page commits, and resolves as disposed when it
	// never does.
	const lifecycle = ctx.state.getValue().lifecycle;
	const nav: Promise<NavResult> =
		lifecycle === "pre-boot" || lifecycle === "booting"
			? awaitBoot(ctx).then((committed) => {
					return committed
						? startNav(ctx, navInput)
						: { kind: "client-disposed" };
				})
			: startNav(ctx, navInput);
	// The demand registers after the navigation occupies the slot so that no
	// attempt reads the page that the redirect leaves; the redirect's own read
	// satisfies it.
	if (owesRevalidation) {
		input.registerCallRevalidationDemandFn?.(sequenceBeforeRead);
	}
	return {
		kind: "redirect",
		callId: input.callId,
		href: input.href,
		redirectStatus: input.redirectStatus,
		nav,
	};
}
