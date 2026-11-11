import type { MutationResult } from "../index.ts";

/**
 * The state of a mutation hook's calls. While calls are in flight, `latest`
 * is the most recently started call, pending until it settles, after which it
 * holds that call's result; a pending latest call is one of `calls`. When the
 * last call settles, the state keeps the result of the most recently started
 * call.
 */
export type MutationState =
	| { readonly kind: "idle" }
	| { readonly kind: "settled"; readonly result: MutationResult }
	| {
			readonly kind: "active";
			readonly calls: ReadonlySet<object>;
			readonly latest:
				| { readonly kind: "pending"; readonly call: object }
				| { readonly kind: "settled"; readonly result: MutationResult };
	  };

export function createMutationState(): MutationState {
	return { kind: "idle" };
}

export function startMutation(
	state: MutationState,
	call: object,
): MutationState {
	return {
		kind: "active",
		calls: new Set([...(state.kind === "active" ? state.calls : []), call]),
		latest: { kind: "pending", call },
	};
}

export function settleMutation(
	state: MutationState,
	input: { readonly call: object; readonly result: MutationResult },
): MutationState {
	if (state.kind !== "active") {
		return state;
	}
	const calls = new Set(state.calls);
	calls.delete(input.call);
	if (state.latest.kind === "pending" && state.latest.call !== input.call) {
		// The most recently started call is still in flight.
		return { kind: "active", calls, latest: state.latest };
	}
	const latest =
		state.latest.kind === "pending"
			? { kind: "settled" as const, result: input.result }
			: state.latest;
	return calls.size === 0
		? { kind: "settled", result: latest.result }
		: { kind: "active", calls, latest };
}

/** The result that a mutation hook shows: the result of the most recently started call once that call has settled. */
export function readMutationResult(
	state: MutationState,
): MutationResult | undefined {
	switch (state.kind) {
		case "idle":
			return undefined;
		case "settled":
			return state.result;
		case "active":
			return state.latest.kind === "settled"
				? state.latest.result
				: undefined;
	}
}
