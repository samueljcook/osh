import type { ResourceScope } from "primitives-ts/resource-scopes";
import type { PrefetchResult } from "../results.ts";
import { reportBuildSkew } from "./build-skew.ts";
import { getActiveCandidate } from "./candidate-slot.ts";
import type { ClientCtx } from "./ctx.ts";
import { startPreparation, type Preparation } from "./prepare.ts";
import { retireOutputs } from "./retained-outputs.ts";
import { patchState } from "./state.ts";
import { readDestination } from "./targets.ts";

/*
Prefetch preparations are shared by destination. Active handles own speculative
work. Programmatic readiness retains a page until navigation takes it, a later
unheld preparation replaces it, or freshness, invalidation, or refusal retires
it. A caller's abort releases its retained ownership. Prefetch work does not
contribute to the navigation work indicator.
*/

type Observer = { readonly resolve: (result: PrefetchResult) => void };
type EntryOwners = {
	readonly scope: ResourceScope;
	readonly handles: Set<object>;
	readonly observers: Set<Observer>;
};
type ReadyRetention =
	| { readonly kind: "unkept" }
	| { readonly kind: "kept"; readonly keepers: Set<object> };
type PrefetchState =
	| (EntryOwners & { readonly kind: "starting" })
	| (EntryOwners & {
			readonly kind: "preparing";
			readonly preparation: Preparation;
	  })
	| (EntryOwners & {
			readonly kind: "ready";
			readonly preparation: Preparation;
			retention: ReadyRetention;
	  })
	| { readonly kind: "ended"; readonly result: PrefetchResult };

export type PrefetchEntry = {
	readonly destination: string;
	readonly href: string;
	state: PrefetchState;
};

function updatePrefetchState(ctx: ClientCtx): void {
	const preparing = [...ctx.prefetchEntries.values()]
		.filter((entry) => {
			return (
				entry.state.kind === "starting" ||
				entry.state.kind === "preparing"
			);
		})
		.map((entry) => {
			return { href: entry.href };
		});
	const current = ctx.state.getValue().prefetches;
	if (
		current.length === preparing.length &&
		current.every((item, index) => item.href === preparing[index]?.href)
	) {
		return;
	}
	patchState(ctx, { prefetches: preparing });
}

function settleObservers(
	observers: Set<Observer>,
	result: PrefetchResult,
): void {
	const pendingObservers = [...observers];
	for (const observer of pendingObservers) {
		observers.delete(observer);
		observer.resolve(result);
	}
}

/** Transfers preparation ownership before publishing or invoking cleanup. */
function endEntry(
	ctx: ClientCtx,
	entry: PrefetchEntry,
	result: PrefetchResult,
): Preparation | undefined {
	const state = entry.state;
	if (state.kind === "ended") {
		return undefined;
	}
	entry.state = { kind: "ended", result };
	if (ctx.prefetchEntries.get(entry.destination) === entry) {
		ctx.prefetchEntries.delete(entry.destination);
	}
	const preparation =
		state.kind === "starting" ? undefined : state.preparation;
	if (result.kind !== "taken-by-nav") {
		preparation?.scope.dispose();
	}
	state.scope.dispose();
	settleObservers(state.observers, result);
	updatePrefetchState(ctx);
	return preparation;
}

export function retirePreparationsAndOutputs(ctx: ClientCtx): void {
	retireOutputs(ctx);
	const entries = [...ctx.prefetchEntries.values()];
	for (const entry of entries) {
		endEntry(ctx, entry, { kind: "superseded" });
	}
}

function retireUnheldEntries(ctx: ClientCtx, keep?: PrefetchEntry): void {
	const entries = [...ctx.prefetchEntries.values()];
	for (const entry of entries) {
		if (
			entry !== keep &&
			entry.state.kind !== "ended" &&
			entry.state.handles.size === 0
		) {
			endEntry(ctx, entry, { kind: "superseded" });
		}
	}
}

function openEntry(ctx: ClientCtx, url: URL, handle: object): PrefetchEntry {
	retireUnheldEntries(ctx);
	const starting: Extract<PrefetchState, { kind: "starting" }> = {
		kind: "starting",
		scope: ctx.scope.createChild(),
		handles: new Set([handle]),
		observers: new Set(),
	};
	const entry: PrefetchEntry = {
		destination: readDestination(url),
		href: url.href,
		state: starting,
	};
	ctx.prefetchEntries.set(entry.destination, entry);
	const preparation = startPreparation(ctx, {
		url,
		requestKind: "prefetch",
		historyState: undefined,
		mayPrestartLoaders: true,
	});
	if (entry.state !== starting || starting.scope.signal.aborted) {
		preparation.scope.dispose();
		if (entry.state === starting) {
			endEntry(ctx, entry, { kind: "superseded" });
		}
		return entry;
	}
	entry.state = { ...starting, kind: "preparing", preparation };
	updatePrefetchState(ctx);
	void preparation.ready.then((ready) => {
		const state = entry.state;
		if (!ready || state.kind !== "preparing") {
			return;
		}
		entry.state = {
			...state,
			kind: "ready",
			retention:
				state.observers.size === 0
					? { kind: "unkept" }
					: { kind: "kept", keepers: new Set() },
		};
		updatePrefetchState(ctx);
		settleObservers(state.observers, { kind: "ready" });
	});
	void preparation.outcome.then((outcome) => {
		if (outcome.kind === "prepared" || entry.state.kind === "ended") {
			return;
		}
		endEntry(
			ctx,
			entry,
			outcome.kind === "aborted"
				? { kind: "superseded" }
				: { kind: "failed", cause: outcome },
		);
		if (outcome.kind === "build-skew") {
			reportBuildSkew(ctx, outcome.skew);
			retirePreparationsAndOutputs(ctx);
		}
	});
	return entry;
}

function releaseHandle(
	ctx: ClientCtx,
	entry: PrefetchEntry,
	handle: object,
): void {
	const state = entry.state;
	if (state.kind === "ended") {
		return;
	}
	state.handles.delete(handle);
	if (state.handles.size > 0) {
		return;
	}
	if (state.kind !== "ready" || state.retention.kind === "unkept") {
		endEntry(ctx, entry, { kind: "superseded" });
		return;
	}
	retireUnheldEntries(ctx, entry);
}

function acquireEntry(ctx: ClientCtx, url: URL, handle: object): PrefetchEntry {
	const entry = ctx.prefetchEntries.get(readDestination(url));
	if (entry === undefined || entry.state.kind === "ended") {
		return openEntry(ctx, url, handle);
	}
	entry.state.handles.add(handle);
	return entry;
}

/** Prepares a destination; abort also releases ownership after readiness. A client that is not ready prefetches nothing. */
export function prefetchDestination(
	ctx: ClientCtx,
	input: { readonly url: URL; readonly signal: AbortSignal | undefined },
): Promise<PrefetchResult> {
	const frame = ctx.state.getValue().frame;
	if (ctx.state.getValue().lifecycle !== "ready" || frame === undefined) {
		return Promise.resolve({ kind: "client-disposed" });
	}
	const destination = readDestination(input.url);
	const candidate = getActiveCandidate(ctx);
	if (
		destination === readDestination(new URL(frame.href)) ||
		(candidate !== undefined &&
			candidate.role.kind !== "revalidation" &&
			destination === readDestination(new URL(candidate.intent.href)))
	) {
		return Promise.resolve({ kind: "not-needed" });
	}
	if (input.signal?.aborted === true) {
		return Promise.resolve({ kind: "superseded" });
	}
	const handle = {};
	const entry = acquireEntry(ctx, input.url, handle);
	const active = entry.state;
	if (active.kind === "ended") {
		return Promise.resolve(active.result);
	}
	const scope = active.scope.createChild();
	const observers = active.observers;
	return new Promise<PrefetchResult>((resolve) => {
		type CallerState =
			| { readonly kind: "pending" }
			| { readonly kind: "keeping"; readonly keepers: Set<object> }
			| { readonly kind: "ended" };
		let state: CallerState = { kind: "pending" };
		function finish(result: PrefetchResult): void {
			const previous = state;
			if (previous.kind === "ended") {
				return;
			}
			state = { kind: "ended" };
			observers.delete(observer);
			if (previous.kind === "keeping") {
				previous.keepers.delete(handle);
			}
			scope.dispose();
			if (previous.kind === "pending") {
				resolve(result);
			}
		}
		const observer: Observer = {
			resolve(result) {
				if (state.kind !== "pending") {
					return;
				}
				if (result.kind !== "ready") {
					finish(result);
					return;
				}
				const current = entry.state;
				if (current.kind !== "ready") {
					finish(
						current.kind === "ended"
							? current.result
							: { kind: "superseded" },
					);
					return;
				}
				if (current.retention.kind === "unkept") {
					current.retention = { kind: "kept", keepers: new Set() };
				}
				const keepers = current.retention.keepers;
				keepers.add(handle);
				state = { kind: "keeping", keepers };
				releaseHandle(ctx, entry, handle);
				resolve({ kind: "ready" });
			},
		};
		function handleAbort(): void {
			const previous = state;
			finish({ kind: "superseded" });
			if (previous.kind === "pending") {
				releaseHandle(ctx, entry, handle);
			} else if (
				previous.kind === "keeping" &&
				entry.state.kind === "ready" &&
				previous.keepers.size === 0 &&
				entry.state.handles.size === 0
			) {
				endEntry(ctx, entry, { kind: "superseded" });
			}
		}
		scope.addCleanup(() => {
			return finish(
				entry.state.kind === "ended"
					? entry.state.result
					: { kind: "superseded" },
			);
		});
		if (scope.signal.aborted) {
			return;
		}
		input.signal?.addEventListener("abort", handleAbort, { once: true });
		scope.addCleanup(() =>
			input.signal?.removeEventListener("abort", handleAbort),
		);
		if (input.signal?.aborted === true) {
			handleAbort();
			return;
		}
		if (entry.state.kind === "ready") {
			observer.resolve({ kind: "ready" });
		} else if (entry.state.kind === "ended") {
			finish(entry.state.result);
		} else {
			entry.state.observers.add(observer);
		}
	});
}

/**
 * Holds speculative work until the returned function releases its ownership.
 * A client that is not ready, such as a client with a failed boot, holds nothing.
 */
export function holdPrefetch(ctx: ClientCtx, url: URL): () => void {
	const frame = ctx.state.getValue().frame;
	if (
		ctx.state.getValue().lifecycle !== "ready" ||
		frame === undefined ||
		readDestination(url) === readDestination(new URL(frame.href))
	) {
		return () => undefined;
	}
	const handle = {};
	const entry = acquireEntry(ctx, url, handle);
	let ownership: "held" | "released" = "held";
	return () => {
		if (ownership === "released") {
			return;
		}
		ownership = "released";
		releaseHandle(ctx, entry, handle);
	};
}

/** Transfers preparation ownership to navigation and ends prefetch observers. */
export function takePrefetchForNav(
	ctx: ClientCtx,
	destination: string,
): { readonly preparation: Preparation } | undefined {
	const entry = ctx.prefetchEntries.get(destination);
	if (entry === undefined) {
		return undefined;
	}
	const preparation = endEntry(ctx, entry, { kind: "taken-by-nav" });
	return preparation === undefined ? undefined : { preparation };
}
