import type { AnyPatternMatch } from "../generated-types.ts";
import type { ClientState, PageState } from "../results.ts";
import type { CoreSlices, CoreState, Frame } from "./core-state.ts";
import type { ClientCtx } from "./ctx.ts";

const pageByFrame = new WeakMap<Frame, PageState>();

function derivePage(frame: Frame, previous: PageState | undefined): PageState {
	const cached = pageByFrame.get(frame);
	if (cached !== undefined) {
		return cached;
	}
	const matches: ReadonlyArray<AnyPatternMatch> = frame.entries.map(
		(entry) => entry.match,
	);
	const reused =
		previous !== undefined &&
		previous.matches.length === matches.length &&
		previous.matches.every((match, index) => match === matches[index])
			? previous.matches
			: matches;
	const page: PageState = {
		href: frame.href,
		historyState: frame.historyState,
		matches: reused,
		error: frame.error?.error,
		commitReason: frame.commitReason,
	};
	pageByFrame.set(frame, page);
	return page;
}

/**
 * Derives the public snapshot. Each slice is the internal slice itself or a
 * value cached by the identity of its source, so a part that did not change
 * keeps its identity, and a snapshot with no changed part is the previous one.
 */
function deriveClientState(
	previous: ClientState | undefined,
	core: CoreState,
): ClientState | undefined {
	if (core.frame === undefined) {
		return undefined;
	}
	const page = derivePage(core.frame, previous?.page);
	const workIndicator =
		previous !== undefined &&
		previous.workIndicator.visible === core.workIndicatorVisible
			? previous.workIndicator
			: { visible: core.workIndicatorVisible };
	if (
		previous !== undefined &&
		previous.page === page &&
		previous.nav === core.nav &&
		previous.lastNavResult === core.lastNavResult &&
		previous.revalidation === core.revalidation &&
		previous.prefetches === core.prefetches &&
		previous.apiEndpointCalls === core.apiEndpointCalls &&
		previous.workIndicator === workIndicator &&
		previous.buildSkew === core.buildSkew
	) {
		return previous;
	}
	return {
		page,
		nav: core.nav,
		lastNavResult: core.lastNavResult,
		revalidation: core.revalidation,
		prefetches: core.prefetches,
		apiEndpointCalls: core.apiEndpointCalls,
		workIndicator,
		buildSkew: core.buildSkew,
	};
}

/** Changes some fields of the internal state and publishes the derived snapshot. */
export function patchState(ctx: ClientCtx, patch: Partial<CoreSlices>): void {
	updateState(ctx, (state) => {
		return { ...state, ...patch };
	});
}

/** Replaces the internal state and publishes the derived snapshot. */
export function updateState(
	ctx: ClientCtx,
	updateFn: (state: CoreState) => CoreState,
): void {
	if (ctx.state.getValue().lifecycle === "disposed") {
		// Nothing changes or notifies after disposal.
		return;
	}
	ctx.state.setValue(updateFn(ctx.state.getValue()));
	// A listener of the internal state can update it again, so the snapshot
	// derives from the latest state.
	ctx.publicState.setValue(
		deriveClientState(ctx.publicState.getValue(), ctx.state.getValue()),
	);
}
