import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "primitives-ts/results";
import type { ClientCtx, RendererOwnership } from "./ctx.ts";

function getRendererOwnership(ctx: ClientCtx): RendererOwnership {
	return ctx.rendererOwnership;
}

function unmount(unmountFn: () => void): void {
	const result = callWithResult(unmountFn);
	if (result.kind === "error") {
		console.error("The renderer could not unmount.", result.error);
	}
}

/** Mounting owns a returned root even when disposal reenters the mount callback. */
export function mountRenderer(
	ctx: ClientCtx,
	root: HTMLElement,
): Result<void, unknown> {
	if (ctx.rendererOwnership.kind !== "unmounted") {
		return createErrorResult(
			new Error("The renderer cannot mount in its ownership state."),
		);
	}
	const mounting = { kind: "mounting" } as const;
	ctx.rendererOwnership = mounting;
	const mounted = callWithResult(() => ctx.renderer.mountRoot(root));
	const ownership = getRendererOwnership(ctx);
	if (ownership.kind === "closing") {
		ctx.rendererOwnership = { kind: "closed" };
		if (mounted.kind === "ok") {
			unmount(mounted.value);
		}
		ownership.finishFn();
		return createErrorResult(
			new Error("The client was disposed while its renderer mounted."),
		);
	}
	if (mounted.kind === "error") {
		ctx.rendererOwnership = { kind: "unmounted" };
		return mounted;
	}
	ctx.rendererOwnership = { kind: "mounted", unmountFn: mounted.value };
	return createSuccessResult(undefined);
}

/** Output resources remain owned until a pending mount returns and its root unmounts. */
export function closeRenderer(ctx: ClientCtx, finishFn: () => void): void {
	const ownership = ctx.rendererOwnership;
	if (ownership.kind === "mounting") {
		ctx.rendererOwnership = { kind: "closing", finishFn };
		return;
	}
	if (ownership.kind === "closing") {
		return;
	}
	ctx.rendererOwnership = { kind: "closed" };
	if (ownership.kind === "mounted") {
		unmount(ownership.unmountFn);
	}
	finishFn();
}
