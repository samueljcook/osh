import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../../results/index.ts";

/** Cancellation preserves the signal's reason; operation failure preserves its cause. */
export type AbortableOperationError =
	| { readonly kind: "aborted"; readonly reason: unknown }
	| { readonly kind: "failed"; readonly cause: unknown };

/**
 * Runs an operation and waits for its result or cancellation. A signal that has
 * already aborted prevents startup. The first observed completion wins, and a
 * late rejection remains handled. Cancellation removes the abort listener but
 * does not stop an operation that ignores its signal.
 */
export function awaitWithAbort<Value>(input: {
	readonly signal: AbortSignal;
	readonly operationFn: (signal: AbortSignal) => Value;
}): Promise<Result<Awaited<Value>, AbortableOperationError>> {
	const { signal, operationFn } = input;
	if (signal.aborted) {
		return Promise.resolve(
			createErrorResult({ kind: "aborted", reason: signal.reason }),
		);
	}
	return new Promise((resolve) => {
		type OperationResult = Result<Awaited<Value>, AbortableOperationError>;
		let state:
			| {
					readonly kind: "waiting";
					readonly resolve: (result: OperationResult) => void;
			  }
			| { readonly kind: "settled" } = { kind: "waiting", resolve };
		function settle(
			result: Result<Awaited<Value>, AbortableOperationError>,
		): void {
			if (state.kind === "settled") {
				return;
			}
			const waiting = state;
			state = { kind: "settled" };
			signal.removeEventListener("abort", handleAbort);
			waiting.resolve(result);
		}
		function handleAbort(): void {
			settle(
				createErrorResult({ kind: "aborted", reason: signal.reason }),
			);
		}
		signal.addEventListener("abort", handleAbort, { once: true });
		const started = callWithResult(() => operationFn(signal));
		if (started.kind === "error") {
			settle(createErrorResult({ kind: "failed", cause: started.error }));
			return;
		}
		// Both handlers must attach even when the operation aborts before returning.
		void Promise.resolve(started.value).then(
			(value) => settle(createSuccessResult(value)),
			(cause: unknown) =>
				settle(createErrorResult({ kind: "failed", cause })),
		);
	});
}
