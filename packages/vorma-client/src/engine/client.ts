import { createObservableValue } from "primitives-ts/observable-values";
import { createResourceScope } from "primitives-ts/resource-scopes";
import { rememberScroll } from "../browser/history.ts";
import { readScrollPosition } from "../browser/scroll.ts";
import type {
	BootResult,
	ClientState,
	CoreOptions,
	VormaClient,
} from "../results.ts";
import { closeBootMetadata } from "./boot-metadata.ts";
import { bootClient } from "./boot.ts";
import { createClientLoaderRegistry } from "./client-loaders.ts";
import { INITIAL_CORE_STATE } from "./core-state.ts";
import type { ClientCtx, RendererBinding } from "./ctx.ts";
import { getCurrentCtx, setCurrentCtx } from "./current.ts";
import { reportFailure } from "./events.ts";
import { INITIAL_FRESHNESS, retireDemand } from "./freshness.ts";
import { closeActiveCandidate } from "./nav.ts";
import { retirePreparationsAndOutputs } from "./prefetch.ts";
import { closeRenderer } from "./renderer-ownership.ts";
import { applyFreshness, cancelScheduledRevalidation } from "./revalidation.ts";
import { updateState } from "./state.ts";

/*
Creating a client starts nothing and touches no document. A document has one
client: a new client becomes the current one only when no live client exists,
and a client that is not the current one refuses to boot.
*/

function createClientCtx(input: {
	readonly options: CoreOptions;
	readonly renderer: RendererBinding;
}): ClientCtx {
	function reportListenerError(cause: unknown): void {
		reportFailure(ctx, "listener", cause);
	}
	function reportOutputCleanupError(cause: unknown): void {
		reportFailure(ctx, "client-output-cleanup", cause);
	}
	const ctx: ClientCtx = {
		options: input.options,
		renderer: input.renderer,
		outputResourceScope: createResourceScope({
			onCleanupErrorFn: reportOutputCleanupError,
		}),
		releaseFrameOutputs: () => undefined,
		scope: createResourceScope({
			onCleanupErrorFn: (error) => {
				console.error("A Vorma cleanup failed.", error);
			},
		}),
		state: createObservableValue({
			initialValue: INITIAL_CORE_STATE,
			compareValuesForEqualityFn: ({ left, right }) =>
				Object.is(left, right),
			onListenerErrorFn: reportListenerError,
		}),
		publicState: createObservableValue<ClientState | undefined>({
			initialValue: undefined,
			compareValuesForEqualityFn: ({ left, right }) =>
				Object.is(left, right),
			onListenerErrorFn: reportListenerError,
		}),
		eventListeners: new Set(),
		navStartListeners: new Set(),
		loaders: createClientLoaderRegistry((cause) => {
			reportFailure(ctx, "pattern-matching", cause);
		}),
		retainedOutputs: new Map(),
		prefetchEntries: new Map(),
		bootMetadata: { kind: "unread", waiters: [] },
		lastSequence: 0,
		generation: 0,
		frameRevision: 0,
		scrollApplication: { kind: "unavailable" },
		navMachine: { kind: "idle" },
		freshness: INITIAL_FRESHNESS,
		revalidationScheduling: { kind: "uninitialized" },
		rendererOwnership: { kind: "unmounted" },
		watchedQueries: new Set(),
		runsBySupersedeKey: new Map(),
		endpointRuns: new Set(),
		lastSkewKey: undefined,
	};
	return ctx;
}

/**
 * Resolves true when the client is ready, and false when its boot failed or it
 * was disposed. While the client boots, it waits until the lifecycle leaves
 * `pre-boot` and `booting`, and then reads the lifecycle after the synchronous
 * rest of boot, which mounts the renderer and attaches the browser listeners
 * and can still fail.
 */
export function awaitBoot(ctx: ClientCtx): Promise<boolean> {
	const lifecycle = ctx.state.getValue().lifecycle;
	if (lifecycle !== "pre-boot" && lifecycle !== "booting") {
		return Promise.resolve(lifecycle === "ready");
	}
	return new Promise((resolve) => {
		const unsubscribe = ctx.state.subscribe(({ value }) => {
			if (
				value.lifecycle === "pre-boot" ||
				value.lifecycle === "booting"
			) {
				return;
			}
			unsubscribe();
			queueMicrotask(() => {
				resolve(ctx.state.getValue().lifecycle === "ready");
			});
		});
	});
}

/**
 * Creates the document's Vorma client. The core never inspects a renderer: the
 * adapter supplies the two operations of `RendererBinding`.
 */
export function createClientCore(input: {
	readonly options: CoreOptions;
	readonly renderer: RendererBinding;
}): VormaClient {
	const ctx = createClientCtx(input);
	const liveClient = getCurrentCtx();
	if (
		liveClient === undefined ||
		liveClient.state.getValue().lifecycle === "disposed"
	) {
		setCurrentCtx(ctx);
	}

	/** Stops everything that the client owns. A failed boot keeps its failure screen mounted. */
	function closeClient(options: { readonly unmount: boolean }): void {
		const frame = ctx.state.getValue().frame;
		if (frame?.entryKey !== undefined) {
			// The committed entry's scroll is saved before anything is torn down.
			rememberScroll(frame.entryKey, readScrollPosition());
		}
		closeActiveCandidate(ctx);
		cancelScheduledRevalidation(ctx);
		for (const query of Array.from(ctx.watchedQueries)) {
			query.stopWatching();
		}
		applyFreshness(
			ctx,
			retireDemand(ctx.freshness, { kind: "client-disposed" }),
		);
		retirePreparationsAndOutputs(ctx);
		for (const run of Array.from(ctx.endpointRuns)) {
			run.interrupt("disposed");
		}
		closeBootMetadata(ctx);
		ctx.scope.dispose();
		function finishOutputs(): void {
			ctx.releaseFrameOutputs();
			ctx.outputResourceScope.dispose();
		}
		if (options.unmount) {
			closeRenderer(ctx, finishOutputs);
		} else {
			finishOutputs();
		}
	}

	return {
		boot: async (): Promise<BootResult> => {
			if (ctx.state.getValue().lifecycle !== "pre-boot") {
				return {
					kind: "failure",
					msg: "A client boots once, and never after it is disposed.",
					cause: new Error(
						"The client already started or was disposed.",
					),
				};
			}
			if (getCurrentCtx() !== ctx) {
				return {
					kind: "failure",
					msg: "Another Vorma client already exists in this document.",
					cause: new Error(
						"A document has one client, and this one is not the document's client.",
					),
				};
			}
			updateState(ctx, (state) => {
				return {
					...state,
					lifecycle: "booting",
					frame: undefined,
				};
			});
			return bootClient(ctx, {
				teardownFn: closeClient,
			});
		},
		dispose: () => {
			const lifecycle = ctx.state.getValue().lifecycle;
			if (lifecycle === "disposed") {
				return;
			}
			// Disposal notifies the internal state's subscribers once, with the
			// disposed lifecycle, and publishes no public state: nothing changes
			// or notifies after it.
			ctx.state.setValue({
				...ctx.state.getValue(),
				lifecycle: "disposed",
			});
			// A failed boot already closed everything except its screen.
			closeClient({ unmount: true });
		},
	};
}
