import type { ClientCtx } from "./ctx.ts";

/*
A document has exactly one Vorma client: it owns the document's history, scroll
position, root element, and head. The commands therefore act on the client that
is current, and no binding module is needed.
*/

let current: ClientCtx | undefined;

export function setCurrentCtx(ctx: ClientCtx | undefined): void {
	current = ctx;
}

export function getCurrentCtx(): ClientCtx | undefined {
	return current;
}

/** The current client. A command called before any client exists is a programming error. */
export function requireCtx(): ClientCtx {
	if (current === undefined) {
		throw new Error(
			"No Vorma client exists. Create one with createVormaClient before calling a Vorma command.",
		);
	}
	return current;
}
