import {
	createObservableValue,
	type ObservableValueChange,
} from "../../observable-values/index.ts";
import { callWithResult } from "../../results/index.ts";

/** A source reports its active registration count and synchronous changes. */
export type ActivitySource = {
	/** Returns the current count. */
	readonly getActiveCount: () => number;
	/** Subscribes to future count changes and returns an independent remover. */
	readonly subscribe: (
		listener: (change: ObservableValueChange<number>) => void,
	) => () => void;
};

/** A tracker counts work independently of any indicator's lifetime or timing. */
export type ActivityTracker = ActivitySource & {
	/** Registers one activity and returns an idempotent function that ends it. */
	readonly beginActivity: () => () => void;
	/**
	 * Tracks settlement and returns the supplied promise without replacing it.
	 * Tracking attaches a rejection handler to the promise, so the runtime does
	 * not report an unhandled rejection for a tracked promise even when the
	 * caller attaches no handler of its own.
	 */
	readonly trackPromise: <TrackedPromise extends PromiseLike<unknown>>(
		promise: TrackedPromise,
	) => TrackedPromise;
};

/**
 * Creates an idle tracker without timers or global state. Every registration
 * counts separately, including repeated tracking of the same promise. Listeners
 * and error reporting must be synchronous. Listener failures go to the supplied
 * handler. If that handler throws, a manual operation throws after restoring its
 * bookkeeping; a promise-settlement callback reports the defect in a microtask
 * without replacing the tracked promise's outcome.
 */
export function createActivityTracker(input: {
	readonly onListenerErrorFn: (cause: unknown) => void;
}): ActivityTracker {
	const count = createObservableValue({
		initialValue: 0,
		compareValuesForEqualityFn: ({ left, right }) => left === right,
		onListenerErrorFn: input.onListenerErrorFn,
	});

	function beginActivity(): () => void {
		let state: "active" | "finished" = "active";
		function endActivity(): void {
			if (state === "finished") {
				return;
			}
			state = "finished";
			count.updateValue((value) => value - 1);
		}
		const started = callWithResult(() => {
			count.updateValue((value) => value + 1);
		});
		if (started.kind === "error") {
			const ended = callWithResult(endActivity);
			if (ended.kind === "error") {
				throw new AggregateError(
					[started.error, ended.error],
					"Activity listener error reporting failed during registration and cleanup.",
				);
			}
			throw started.error;
		}
		return endActivity;
	}

	function trackPromise<TrackedPromise extends PromiseLike<unknown>>(
		promise: TrackedPromise,
	): TrackedPromise {
		const endActivity = beginActivity();
		function finishTracking(): void {
			const ended = callWithResult(endActivity);
			if (ended.kind === "error") {
				queueMicrotask(() => {
					throw ended.error;
				});
			}
		}
		const attached = callWithResult(() => {
			void Promise.resolve(promise).then(finishTracking, finishTracking);
		});
		if (attached.kind === "error") {
			const ended = callWithResult(endActivity);
			if (ended.kind === "error") {
				throw new AggregateError(
					[attached.error, ended.error],
					"Promise settlement attachment and activity cleanup failed.",
				);
			}
			throw attached.error;
		}
		return promise;
	}

	return {
		getActiveCount: count.getValue,
		subscribe: count.subscribe,
		beginActivity,
		trackPromise,
	};
}
