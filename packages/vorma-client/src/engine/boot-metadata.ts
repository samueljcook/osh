import type { BootMetadata, ClientCtx } from "./ctx.ts";

/**
 * The document's build metadata. Every caller runs after boot has read it:
 * page reads start only while the client is ready, and an endpoint call builds
 * its request only while the client is ready or after `awaitBootMetadata`
 * resolved true. A call before that point is a defect in the client, so it
 * throws.
 */
export function getBootMetadata(ctx: ClientCtx): BootMetadata {
	const state = ctx.bootMetadata;
	if (state.kind !== "read") {
		throw new Error(
			"Boot metadata must be read before request or page preparation.",
		);
	}
	return state.metadata;
}

/** Records the metadata that boot read and resolves the waiters with true. */
export function recordBootMetadata(
	ctx: ClientCtx,
	metadata: BootMetadata,
): void {
	const state = ctx.bootMetadata;
	if (state.kind !== "unread") {
		return;
	}
	ctx.bootMetadata = { kind: "read", metadata };
	for (const waiter of state.waiters.splice(0)) {
		waiter(true);
	}
}

/** Records that the metadata will not be read, because boot failed first or the client was disposed, and resolves the waiters with false. */
export function closeBootMetadata(ctx: ClientCtx): void {
	const state = ctx.bootMetadata;
	if (state.kind !== "unread") {
		return;
	}
	ctx.bootMetadata = { kind: "unavailable" };
	for (const waiter of state.waiters.splice(0)) {
		waiter(false);
	}
}

/** Resolves true once boot has read the metadata, and false when it never will. */
export function awaitBootMetadata(ctx: ClientCtx): Promise<boolean> {
	const state = ctx.bootMetadata;
	if (state.kind !== "unread") {
		return Promise.resolve(state.kind === "read");
	}
	return new Promise((resolve) => {
		state.waiters.push(resolve);
	});
}
