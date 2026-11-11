import type { BuildSkew } from "../results.ts";
import type { ClientCtx } from "./ctx.ts";
import { patchState } from "./state.ts";

/**
 * Records build skew in the state. The loaded build ID never changes. A skew
 * with the same request kind, requested href, and server build ID as the last
 * recorded skew is not recorded again; a skew that equals an earlier one but
 * not the last one is recorded again.
 */
export function reportBuildSkew(ctx: ClientCtx, skew: BuildSkew): void {
	const key = `${skew.requestKind} ${skew.requestedHref} ${skew.serverBuildId ?? ""}`;
	if (ctx.lastSkewKey === key) {
		return;
	}
	ctx.lastSkewKey = key;
	patchState(ctx, { buildSkew: skew });
}
