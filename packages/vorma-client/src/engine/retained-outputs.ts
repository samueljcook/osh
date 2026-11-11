import { retainClientOutputs } from "./client-output-resources.ts";
import type { ClientCtx, RetainedOutputs } from "./ctx.ts";
import type { PreparedPage } from "./prepare.ts";

const MAX_TIMER_MILLIS = 2_147_483_647;

/*
Page outputs that a view's `staleTimeMillis` retains by destination. A visit
inside the window commits them without a request or a client loader run, and
keeps the time of the read. A demand, an invalidation, a build refusal, and a
development update retire them early.
*/

export function retainOutputs(
	ctx: ClientCtx,
	input: { readonly prepared: PreparedPage; readonly ttlMillis: number },
): void {
	const { prepared } = input;
	const previous = ctx.retainedOutputs.get(prepared.destination);
	const releaseOutputs = retainClientOutputs(prepared.entries);
	const retained: RetainedOutputs = {
		prepared,
		expiresAt: Date.now() + input.ttlMillis,
		state: {
			kind: "retained",
			timer: setTimeout(
				expireRetainedOutputs,
				Math.min(input.ttlMillis, MAX_TIMER_MILLIS),
			),
			releaseOutputs,
		},
	};
	ctx.retainedOutputs.set(prepared.destination, retained);
	if (previous !== undefined) {
		releaseRetainedOutputs(previous);
	}

	function expireRetainedOutputs(): void {
		if (
			retained.state.kind === "released" ||
			ctx.retainedOutputs.get(prepared.destination) !== retained
		) {
			return;
		}
		const remainingMillis = retained.expiresAt - Date.now();
		if (remainingMillis > 0) {
			retained.state = {
				...retained.state,
				timer: setTimeout(
					expireRetainedOutputs,
					Math.min(remainingMillis, MAX_TIMER_MILLIS),
				),
			};
			return;
		}
		ctx.retainedOutputs.delete(prepared.destination);
		releaseRetainedOutputs(retained);
	}
}

/** The retained page for a destination while its window is open. */
export function findRetainedOutputs(
	ctx: ClientCtx,
	destination: string,
): PreparedPage | undefined {
	const retained = ctx.retainedOutputs.get(destination);
	if (retained === undefined) {
		return undefined;
	}
	if (
		retained.expiresAt <= Date.now() ||
		retained.prepared.generation !== ctx.generation
	) {
		ctx.retainedOutputs.delete(destination);
		releaseRetainedOutputs(retained);
		return undefined;
	}
	return retained.prepared;
}

export function retireOutputs(ctx: ClientCtx): void {
	const retired = [...ctx.retainedOutputs.values()];
	ctx.retainedOutputs.clear();
	for (const retained of retired) {
		releaseRetainedOutputs(retained);
	}
}

function releaseRetainedOutputs(retained: RetainedOutputs): void {
	const state = retained.state;
	if (state.kind === "released") {
		return;
	}
	retained.state = { kind: "released" };
	clearTimeout(state.timer);
	state.releaseOutputs();
}
