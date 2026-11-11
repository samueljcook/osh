/*
The interface between the core and a renderer adapter. It is not a public entry
of the package: the adapters live in this package and import it by relative
path. Nothing here names a renderer, so a new adapter needs no change to the
core.
*/

import { moveFocusAfterScroll } from "./browser/focus.ts";
import { applyScroll } from "./browser/scroll.ts";
import type { Frame, FrameError, RouteEntry } from "./engine/core-state.ts";
import { requireCtx } from "./engine/current.ts";
import { reportFailure } from "./engine/events.ts";
import { toBrowserPageError } from "./engine/page-error.ts";
import { updateState } from "./engine/state.ts";

export { createClientCore } from "./engine/client.ts";
export type { RendererBinding } from "./engine/ctx.ts";
export type { Frame, FrameError, RouteEntry };
export { toBrowserPageError };
export { createLinkIntent, type LinkIntent } from "./engine/link-intent.ts";
export {
	computeLinkState,
	selectLinkStateFacts,
	type LinkStateFacts,
} from "./engine/link-state.ts";
export { checkNavigableHref, resolveTargetHref } from "./engine/targets.ts";
export { brandRendererNeutralView, checkRendererNeutralView } from "./views.ts";

/** The committed frame, which a renderer turns into its own reactive form. */
export function getRenderFrame(): Frame | undefined {
	return requireCtx().state.getValue().frame;
}

/** Calls the listener whenever a new frame commits, or the frame's error changes. */
export function subscribeToRenderFrame(
	listener: (frame: Frame | undefined) => void,
): () => void {
	return requireCtx().state.subscribe(
		({ value: state, previousValue: previous }) => {
			if (state.frame !== previous.frame) {
				listener(state.frame);
			}
		},
	);
}

/**
 * Records that a position of the frame failed to render. The failure becomes
 * the frame's error when no earlier position already failed.
 */
export function reportRenderFailure(input: {
	readonly revision: number;
	readonly positionIndex: number;
	readonly pattern: string | undefined;
	readonly cause: unknown;
}): void {
	const ctx = requireCtx();
	reportFailure(ctx, "render", input.cause);
	updateState(ctx, (state) => {
		const frame = state.frame;
		if (
			frame === undefined ||
			frame.revision !== input.revision ||
			(frame.error !== undefined &&
				frame.error.positionIndex <= input.positionIndex)
		) {
			return state;
		}
		return {
			...state,
			frame: {
				...frame,
				error: {
					positionIndex: input.positionIndex,
					pattern: input.pattern,
					error: toBrowserPageError({
						kind: "render-failure",
						pattern: input.pattern,
						cause: input.cause,
					}),
				},
			},
		};
	});
}

/** Reports a link with a target that cannot be encoded. */
export function reportLinkFailure(cause: unknown): void {
	reportFailure(requireCtx(), "link", cause);
}

/**
 * Applies the frame's scroll intent once, after the renderer has rendered the
 * destination depth. A later call for the same frame does nothing. For a frame
 * that a navigation or a history traversal committed, focus then follows the
 * scroll that the intent performed, as a browser's document navigation sets
 * it (`moveFocusAfterScroll`); a frame without a scroll intent, a scroll that
 * did not happen, and a frame of boot keep focus where it is.
 */
export function consumeScrollIntent(revision: number): void {
	const ctx = requireCtx();
	const frame = ctx.state.getValue().frame;
	const scroll = ctx.scrollApplication;
	if (
		frame === undefined ||
		frame.revision !== revision ||
		frame.scrollIntent === undefined ||
		scroll.kind !== "pending" ||
		scroll.revision !== revision
	) {
		return;
	}
	ctx.scrollApplication = { kind: "applied", revision };
	const applied = applyScroll(frame.scrollIntent);
	if (applied.kind === "error") {
		reportFailure(ctx, "nav", applied.error);
		return;
	}
	if (frame.commitReason === "nav" || frame.commitReason === "popstate") {
		const focused = moveFocusAfterScroll(applied.value);
		if (focused.kind === "error") {
			reportFailure(ctx, "nav", focused.error);
		}
	}
}
