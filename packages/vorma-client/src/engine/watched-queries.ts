import { createObservableValue } from "primitives-ts/observable-values";
import {
	createSharedResource,
	type SharedResource,
} from "primitives-ts/shared-resources";
import { compareValuesForStructuralEquality } from "primitives-ts/value-comparison";
import { createRandomId } from "../foundation/random-id.ts";
import type {
	QueryApiEndpointCall,
	RawApiEndpointCall,
} from "../generated-types.ts";
import type {
	ApiEndpointCallResult,
	MutationResult,
	WatchedQuery,
	WatchedQueryState,
} from "../results.ts";
import { awaitBoot } from "./client.ts";
import type { ClientCtx, QueryRuntime } from "./ctx.ts";
import { buildEndpointRequest, callApiEndpointRaw } from "./endpoints.ts";
import { acknowledgeQuery, releaseQuery } from "./freshness.ts";
import { applyFreshness } from "./revalidation.ts";

/*
A watched query is a read of a query endpoint that stays current while
something watches it. Vorma fetches it when watching starts, again when its call
changes, and again on every revalidation, and stops when watching stops.
Watchers share a request when its method, URL, body, request options, caller
signal, and work-indicator preference match. A supersede key starts a separate
request so that it can cancel the preceding call. A revalidation is complete
only when every watched query has fetched again. A declared error counts as a
fresh answer; a failure without one fails the revalidation. A fetch that a
later call superseded, or that its caller's signal aborted, gives no answer, so
the revalidation stops waiting for that query.
*/

type SharedFetch = {
	readonly epoch: number;
	readonly options: {
		readonly requestOptions: object;
		readonly signal: AbortSignal | undefined;
		readonly skipWorkIndicator: boolean;
	};
	readonly promise: Promise<ApiEndpointCallResult | MutationResult>;
	readonly forget: () => void;
	/**
	 * The request that the watchers hold, which is running while it is held.
	 * The release of the last hold aborts the request, and the request's
	 * settlement ends the holds without aborting it.
	 */
	readonly sharedRequest: SharedResource;
};

const sharedByCtx = new WeakMap<ClientCtx, Map<string, Set<SharedFetch>>>();

function getSharedFetches(ctx: ClientCtx): Map<string, Set<SharedFetch>> {
	let shared = sharedByCtx.get(ctx);
	if (shared === undefined) {
		shared = new Map();
		sharedByCtx.set(ctx, shared);
	}
	return shared;
}

/** Acquires a request before its dispatch or any notification can call application code. */
function holdQueryFetch(ctx: ClientCtx, call: RawApiEndpointCall) {
	const built = buildEndpointRequest(ctx, call);
	const options = {
		requestOptions: {
			...call.requestOptions,
			headers:
				built.kind === "ok" ? [...built.value.request.headers] : [],
		},
		signal: call.signal,
		skipWorkIndicator: call.skipWorkIndicator === true,
	};
	// A superseding call must start its own run, even when its request matches.
	const key =
		built.kind === "ok" &&
		call.supersedeKey === undefined &&
		(call.method.toUpperCase() === "GET" ||
			built.value.bodyKey !== undefined)
			? `${call.method.toUpperCase()} ${built.value.href} ${built.value.bodyKey ?? ""}`
			: undefined;
	const sharedMap = getSharedFetches(ctx);
	const bucket = key === undefined ? undefined : sharedMap.get(key);
	for (const fetch of bucket ?? []) {
		if (
			fetch.epoch !== ctx.freshness.epoch ||
			!compareValuesForStructuralEquality({
				left: fetch.options,
				right: options,
			})
		) {
			continue;
		}
		const hold = fetch.sharedRequest.hold();
		// The type alone requires this check: every request in a bucket is held
		// because settlement and cancellation remove a request from its bucket
		// in the same step.
		if (hold.kind === "held") {
			return { promise: fetch.promise, release: hold.release };
		}
	}
	const controller = new AbortController();
	const entries = bucket ?? new Set<SharedFetch>();
	const { sharedResource: sharedRequest, releaseCreatorHold } =
		createSharedResource({
			disposeFn: () => {
				created.forget();
				controller.abort();
			},
		});
	const created: SharedFetch = {
		epoch: ctx.freshness.epoch,
		options,
		sharedRequest,
		forget: () => {
			entries.delete(created);
			if (
				key !== undefined &&
				entries.size === 0 &&
				sharedMap.get(key) === entries
			) {
				sharedMap.delete(key);
			}
		},
		promise: Promise.resolve().then(() => {
			return callApiEndpointRaw(
				ctx,
				{
					...call,
					signal:
						call.signal === undefined
							? controller.signal
							: AbortSignal.any([call.signal, controller.signal]),
				},
				{ endsWithGeneration: false, builtRequest: built },
			);
		}),
	};
	if (key !== undefined) {
		entries.add(created);
		sharedMap.set(key, entries);
	}
	void created.promise.then(() => {
		created.sharedRequest.endWithoutDisposal();
		created.forget();
	});
	return { promise: created.promise, release: releaseCreatorHold };
}

/**
 * A query call without its signal, which identifies the query that a watcher
 * fetches. Two calls are the same query when their identities are equal by
 * contents, so a render that builds its call anew, including its signal, keeps
 * the watched query.
 */
type QueryIdentity = Omit<RawApiEndpointCall, "signal">;

function toQueryIdentity(call: QueryApiEndpointCall): QueryIdentity {
	const { signal: _signal, ...identity } = call as RawApiEndpointCall;
	return identity;
}

function checkFreshAnswer(
	result: ApiEndpointCallResult | MutationResult,
): boolean {
	return (
		result.kind === "success" ||
		result.kind === "declared-error" ||
		result.kind === "rejection" ||
		result.kind === "redirect"
	);
}

export function watchQuery(
	ctx: ClientCtx,
	initialCall: QueryApiEndpointCall | undefined,
): WatchedQuery {
	type HeldFetch = ReturnType<typeof holdQueryFetch>;
	type ActiveWatchFields = {
		readonly token: symbol;
		readonly call: QueryIdentity;
		/**
		 * The signal of the latest call. Each fetch starts with it, so a call
		 * that differs from the watched query only in its signal applies to
		 * the next fetch without starting a fetch of its own.
		 */
		readonly callerSignal: AbortSignal | undefined;
		readonly result: WatchedQueryState["result"];
	};
	type WatchState =
		| { readonly kind: "disabled" }
		| ({ readonly kind: "idle" } & ActiveWatchFields)
		| ({ readonly kind: "waiting-boot" } & ActiveWatchFields)
		| ({ readonly kind: "acquiring" } & ActiveWatchFields)
		| ({
				readonly kind: "fetching";
				readonly held: HeldFetch;
				readonly sequence: number;
		  } & ActiveWatchFields)
		| {
				readonly kind: "stopped";
				readonly result: WatchedQueryState["result"];
		  };
	let state: WatchState =
		initialCall === undefined
			? { kind: "disabled" }
			: {
					kind: "idle",
					token: Symbol(),
					call: toQueryIdentity(initialCall),
					callerSignal: initialCall.signal,
					result: undefined,
				};
	const snapshots = createObservableValue<WatchedQueryState>({
		initialValue: { result: undefined, isFetching: false },
		compareValuesForEqualityFn: ({ left, right }) => {
			return (
				left.result === right.result &&
				left.isFetching === right.isFetching
			);
		},
		onListenerErrorFn: (cause) =>
			console.error("A Vorma watched query listener failed.", cause),
	});
	const id = createRandomId();

	function publish(): void {
		snapshots.setValue({
			result: state.kind === "disabled" ? undefined : state.result,
			isFetching:
				state.kind === "waiting-boot" ||
				state.kind === "acquiring" ||
				state.kind === "fetching",
		});
	}

	function checkFetching(): boolean {
		return state.kind === "fetching";
	}

	function startFetch(): boolean {
		const previous = state;
		if (previous.kind === "stopped") {
			return false;
		}
		if (previous.kind === "disabled") {
			publish();
			return false;
		}
		const token = Symbol();
		const { call } = previous;
		state = {
			kind: "acquiring",
			token,
			call,
			callerSignal: previous.callerSignal,
			result: previous.result,
		};
		if (previous.kind === "fetching") {
			previous.held.release();
		}
		if (state.kind !== "acquiring" || state.token !== token) {
			return checkFetching();
		}
		const lifecycle = ctx.state.getValue().lifecycle;
		if (lifecycle === "pre-boot" || lifecycle === "booting") {
			state = {
				kind: "waiting-boot",
				token,
				call,
				callerSignal: state.callerSignal,
				result: state.result,
			};
			publish();
			void awaitBoot(ctx).then((committed) => {
				if (state.kind !== "waiting-boot" || state.token !== token) {
					return;
				}
				if (committed) {
					startFetch();
				} else {
					state = {
						kind: "idle",
						token: Symbol(),
						call,
						callerSignal: state.callerSignal,
						result: { kind: "client-disposed" },
					};
					publish();
				}
			});
			return false;
		}
		if (lifecycle !== "ready") {
			state = {
				kind: "idle",
				token: Symbol(),
				call,
				callerSignal: state.callerSignal,
				result: { kind: "client-disposed" },
			};
			publish();
			return false;
		}
		const sequence = ++ctx.lastSequence;
		const signal = state.callerSignal;
		const held = holdQueryFetch(
			ctx,
			signal === undefined ? call : { ...call, signal },
		);
		if (state.kind !== "acquiring" || state.token !== token) {
			held.release();
			return checkFetching();
		}
		state = {
			kind: "fetching",
			token,
			call,
			callerSignal: state.callerSignal,
			result: state.result,
			held,
			sequence,
		};
		void held.promise.then((answer) => {
			if (state.kind !== "fetching" || state.token !== token) {
				return;
			}
			const settledToken = Symbol();
			const nextResult =
				answer.kind !== "aborted" && answer.kind !== "superseded"
					? (answer as ApiEndpointCallResult)
					: state.result;
			state = {
				kind: "idle",
				token: settledToken,
				call,
				callerSignal: state.callerSignal,
				result: nextResult,
			};
			held.release();
			publish();
			if (
				state.kind !== "idle" ||
				state.token !== settledToken ||
				answer.kind === "client-disposed"
			) {
				return;
			}
			if (answer.kind === "superseded" || answer.kind === "aborted") {
				// The fetch ended without an answer, and the watcher has no other
				// fetch running, so the query releases its obligation.
				applyFreshness(ctx, releaseQuery(ctx.freshness, id));
				return;
			}
			applyFreshness(
				ctx,
				acknowledgeQuery(ctx.freshness, {
					id,
					sequence,
					success: checkFreshAnswer(answer),
					cause: answer,
				}),
			);
		});
		publish();
		return checkFetching();
	}

	const runtime: QueryRuntime = {
		id,
		fetchForDemand: startFetch,
		stopWatching: stop,
	};
	function stop(): void {
		const previous = state;
		if (previous.kind === "stopped") {
			return;
		}
		state = {
			kind: "stopped",
			result: previous.kind === "disabled" ? undefined : previous.result,
		};
		ctx.watchedQueries.delete(runtime);
		if (previous.kind === "fetching") {
			previous.held.release();
		}
		publish();
		applyFreshness(ctx, releaseQuery(ctx.freshness, id));
	}

	ctx.watchedQueries.add(runtime);
	startFetch();
	return {
		getState: snapshots.getValue,
		subscribe: (listener) =>
			snapshots.subscribe((change) => listener(change.value)),
		setCall(next) {
			const previous = state;
			if (previous.kind === "stopped") {
				return;
			}
			const call =
				previous.kind === "disabled" ? undefined : previous.call;
			const nextCall =
				next === undefined ? undefined : toQueryIdentity(next);
			if (
				compareValuesForStructuralEquality({
					left: call,
					right: nextCall,
				})
			) {
				if (
					previous.kind !== "disabled" &&
					previous.callerSignal !== next?.signal
				) {
					state = { ...previous, callerSignal: next?.signal };
				}
				return;
			}
			if (next === undefined || nextCall === undefined) {
				state = { kind: "disabled" };
				if (previous.kind === "fetching") {
					previous.held.release();
				}
				if (state.kind === "disabled") {
					startFetch();
				}
				if (call !== undefined) {
					applyFreshness(ctx, releaseQuery(ctx.freshness, id));
				}
				return;
			}
			const token = Symbol();
			state = {
				kind: "idle",
				token,
				call: nextCall,
				callerSignal: next.signal,
				result:
					previous.kind === "disabled" ? undefined : previous.result,
			};
			if (previous.kind === "fetching") {
				previous.held.release();
			}
			if (state.kind === "idle" && state.token === token) {
				startFetch();
			}
		},
		refetch: () => {
			startFetch();
		},
		stop,
	};
}
