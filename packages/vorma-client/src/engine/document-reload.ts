import type { ClientCtx } from "./ctx.ts";
import { reportFailure } from "./events.ts";
import { retireDemand } from "./freshness.ts";
import { applyFreshness } from "./revalidation.ts";

/** Loads the browser's current URL as a document after client navigation cannot complete. */
export function reloadDocument(ctx: ClientCtx, cause: unknown): void {
	applyFreshness(
		ctx,
		retireDemand(ctx.freshness, { kind: "failure", reason: "page", cause }),
	);
	reportFailure(ctx, "nav", cause);
	window.location.reload();
}
