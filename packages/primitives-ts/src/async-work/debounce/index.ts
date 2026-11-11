import {
	callWithResultAsync,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../../results/index.ts";

export type DebounceConfigError = {
	readonly kind: "invalid-delay";
	readonly delayMillis: number;
};

export type DebouncedCallError =
	| { readonly kind: "cancelled"; readonly reason: unknown }
	| { readonly kind: "failed"; readonly cause: unknown };

/** Calls within one debounce interval share one invocation and result promise. */
export type DebouncedFunction<Args extends Array<unknown>, Value> = ((
	...args: Args
) => Promise<Result<Value, DebouncedCallError>>) & {
	/** Cancels calls that have not started; an invocation already running is independent. */
	readonly cancelPendingCalls: (reason: unknown) => void;
	/** Returns whether calls are waiting for their debounce interval to end. */
	readonly checkPendingCalls: () => boolean;
};

/**
 * Creates a trailing debounce with an explicit delay from zero through
 * 2,147,483,647 milliseconds. Every call restarts the pending timer and replaces
 * its arguments. A zero delay still schedules a timer. Each started invocation
 * completes independently, including when a later batch starts before it ends.
 * The operation's value, promise, or thenable becomes the successful value;
 * returned Result objects are not flattened. Throws and rejections become
 * `failed` with their original cause. Bind methods that require a `this` receiver.
 */
export function createDebouncedFunction<
	Args extends Array<unknown>,
	Value,
>(input: {
	readonly delayMillis: number;
	readonly operationFn: (...args: Args) => Value;
}): Result<DebouncedFunction<Args, Awaited<Value>>, DebounceConfigError> {
	if (
		!Number.isFinite(input.delayMillis) ||
		input.delayMillis < 0 ||
		input.delayMillis > 2_147_483_647
	) {
		return createErrorResult({
			kind: "invalid-delay",
			delayMillis: input.delayMillis,
		});
	}
	type CallResult = Result<Awaited<Value>, DebouncedCallError>;
	type Batch = {
		args: Args;
		readonly promise: Promise<CallResult>;
		readonly resolve: (result: CallResult) => void;
	};
	type PendingBatch = {
		readonly kind: "pending";
		readonly batch: Batch;
		readonly timer: ReturnType<typeof setTimeout>;
	};
	let state: { readonly kind: "idle" } | PendingBatch = { kind: "idle" };

	function startPending(scheduled: PendingBatch): void {
		const { batch } = scheduled;
		state = { kind: "idle" };
		void callWithResultAsync(() => input.operationFn(...batch.args)).then(
			(result) => {
				batch.resolve(
					result.kind === "ok"
						? result
						: createErrorResult({
								kind: "failed",
								cause: result.error,
							}),
				);
			},
		);
	}

	function scheduleCall(...args: Args): Promise<CallResult> {
		let batch: Batch;
		if (state.kind === "pending") {
			clearTimeout(state.timer);
			batch = state.batch;
			batch.args = args;
		} else {
			let resolve!: (result: CallResult) => void;
			const promise = new Promise<CallResult>((resolveBatch) => {
				resolve = resolveBatch;
			});
			batch = { args, promise, resolve };
		}
		const scheduled: PendingBatch = {
			kind: "pending",
			batch,
			timer: setTimeout(() => startPending(scheduled), input.delayMillis),
		};
		state = scheduled;
		return batch.promise;
	}

	return createSuccessResult(
		Object.assign(scheduleCall, {
			checkPendingCalls: () => state.kind === "pending",
			cancelPendingCalls: (reason: unknown) => {
				if (state.kind === "idle") {
					return;
				}
				const pending = state;
				state = { kind: "idle" };
				clearTimeout(pending.timer);
				pending.batch.resolve(
					createErrorResult({ kind: "cancelled", reason }),
				);
			},
		}),
	);
}
