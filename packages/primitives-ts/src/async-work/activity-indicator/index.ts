import {
	createObservableValue,
	type ObservableValueChange,
} from "../../observable-values/index.ts";
import {
	createResourceScope,
	type ResourceScope,
} from "../../resource-scopes/index.ts";
import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../../results/index.ts";
import type { ActivitySource } from "./activity-tracker.ts";

type VisibilityState =
	| { readonly kind: "hidden" }
	| {
			readonly kind: "showing";
			readonly timer: ReturnType<typeof setTimeout>;
	  }
	| { readonly kind: "show-unconfirmed" }
	| { readonly kind: "visible" }
	| {
			readonly kind: "hiding";
			readonly timer: ReturnType<typeof setTimeout>;
	  }
	| { readonly kind: "hide-unconfirmed" };

type ObservingState = {
	readonly kind: "observing";
	readonly sources: Array<ActivitySource>;
	readonly scope: ResourceScope;
	visibility: VisibilityState;
	activityRead: { readonly kind: "idle" } | { readonly kind: "reading" };
};

/**
 * Represents one read across every source. A source that throws from
 * `getActiveCount` counts as inactive, and `failures` records the thrown value.
 */
type ActivityReading =
	| {
			readonly kind: "current";
			readonly isActive: boolean;
			readonly failures: ReadonlyArray<unknown>;
	  }
	| {
			readonly kind: "superseded";
			readonly failures: ReadonlyArray<unknown>;
	  };

function getIndicatorSnapshot(
	visibility: VisibilityState,
): ActivityIndicatorState {
	switch (visibility.kind) {
		case "hidden":
			return Object.freeze({ isActive: false, isVisible: false });
		case "showing":
		case "show-unconfirmed":
			return Object.freeze({ isActive: true, isVisible: false });
		case "visible":
			return Object.freeze({ isActive: true, isVisible: true });
		case "hiding":
		case "hide-unconfirmed":
			return Object.freeze({ isActive: false, isVisible: true });
	}
}

function clearVisibilityTimer(visibility: VisibilityState): void {
	if (visibility.kind === "showing" || visibility.kind === "hiding") {
		clearTimeout(visibility.timer);
	}
}

export {
	createActivityTracker,
	type ActivitySource,
	type ActivityTracker,
} from "./activity-tracker.ts";

export type ActivityIndicatorConfigError = {
	readonly kind: "invalid-delay";
	readonly delay: "show" | "hide";
	readonly delayMillis: number;
};

/** Activity changes immediately; visibility follows the configured delays. */
export type ActivityIndicatorState = {
	readonly isActive: boolean;
	readonly isVisible: boolean;
};

/** An indicator observes activity sources without owning their work. */
export type ActivityIndicator = {
	readonly getState: () => ActivityIndicatorState;
	/** Subscribes to future state changes without an initial notification. */
	readonly subscribe: (
		listener: (
			change: ObservableValueChange<ActivityIndicatorState>,
		) => void,
	) => () => void;
	/**
	 * Stops notifications, removes subscriptions and timers, and resets the state.
	 * If a source's subscription remover throws, disposal finishes and then
	 * throws an AggregateError of the remover failures.
	 */
	readonly dispose: () => void;
};

/**
 * Observes a fixed collection of activity sources. Continuous activity starts a
 * show delay; continuous inactivity starts a hide delay. Changes that retain the
 * same active/inactive state do not restart either delay. Both delays must be
 * finite numbers from zero through 2,147,483,647 milliseconds. Zero still uses a
 * timer, so work that settles before that timer runs does not show the indicator.
 *
 * Each source must publish synchronous count changes and return a synchronous,
 * nonthrowing subscription remover. Listeners and error reporting must also be
 * synchronous. Disposal is idempotent, stops notifications immediately, and
 * resets the readable state to inactive and hidden without a final notification.
 * Sources and tracked operations remain independent of this indicator.
 * Exceptions from a source's `subscribe` propagate, and failed construction
 * removes established subscriptions. When a source's `getActiveCount` throws,
 * that source counts as inactive for that read, and the indicator finishes its
 * state change and then passes the exception to `onListenerErrorFn`. If a
 * source remover throws during disposal, remaining cleanup completes and an
 * AggregateError reports the failures.
 */
export function createActivityIndicator(input: {
	readonly sources: ReadonlyArray<ActivitySource>;
	readonly showDelayMillis: number;
	readonly hideDelayMillis: number;
	readonly onListenerErrorFn: (cause: unknown) => void;
}): Result<ActivityIndicator, ActivityIndicatorConfigError> {
	for (const delay of ["show", "hide"] as const) {
		const delayMillis =
			delay === "show" ? input.showDelayMillis : input.hideDelayMillis;
		if (
			!Number.isFinite(delayMillis) ||
			delayMillis < 0 ||
			delayMillis > 2_147_483_647
		) {
			return createErrorResult({
				kind: "invalid-delay",
				delay,
				delayMillis,
			});
		}
	}
	const sources = [...new Set(input.sources)];
	const showDelayMillis = input.showDelayMillis;
	const hideDelayMillis = input.hideDelayMillis;
	const state = createObservableValue<ActivityIndicatorState>({
		initialValue: Object.freeze({ isActive: false, isVisible: false }),
		compareValuesForEqualityFn: ({ left, right }) => {
			return (
				left.isActive === right.isActive &&
				left.isVisible === right.isVisible
			);
		},
		onListenerErrorFn: input.onListenerErrorFn,
	});
	let lifecycle: ObservingState | { readonly kind: "disposed" } = {
		kind: "observing",
		sources,
		scope: createResourceScope({
			onCleanupErrorFn: (cause) => {
				throw cause;
			},
		}),
		visibility: { kind: "hidden" },
		activityRead: { kind: "idle" },
	};

	function readActivity(observing: ObservingState): ActivityReading {
		const reading = { kind: "reading" } as const;
		observing.activityRead = reading;
		const failures: Array<unknown> = [];
		for (const source of observing.sources) {
			const count = callWithResult(() => source.getActiveCount());
			if (count.kind === "error") {
				failures.push(count.error);
			}
			if (lifecycle !== observing || observing.activityRead !== reading) {
				return { kind: "superseded", failures };
			}
			if (count.kind === "ok" && count.value > 0) {
				observing.activityRead = { kind: "idle" };
				return { kind: "current", isActive: true, failures };
			}
		}
		observing.activityRead = { kind: "idle" };
		return { kind: "current", isActive: false, failures };
	}

	function reportReadFailures(reading: ActivityReading): void {
		const reportingErrors: Array<unknown> = [];
		for (const failure of reading.failures) {
			const reported = callWithResult(() => {
				input.onListenerErrorFn(failure);
			});
			if (reported.kind === "error") {
				reportingErrors.push(reported.error);
			}
		}
		if (reportingErrors.length > 0) {
			throw new AggregateError(
				reportingErrors,
				"The activity-indicator error handler failed.",
			);
		}
	}

	function scheduleVisibility(
		observing: ObservingState,
		target: "visible" | "hidden",
	): void {
		const scheduled: VisibilityState = {
			kind: target === "visible" ? "showing" : "hiding",
			timer: setTimeout(
				() => {
					if (
						lifecycle !== observing ||
						observing.visibility !== scheduled
					) {
						return;
					}
					const unconfirmed: VisibilityState = {
						kind:
							target === "visible"
								? "show-unconfirmed"
								: "hide-unconfirmed",
					};
					observing.visibility = unconfirmed;
					const reading = readActivity(observing);
					if (reading.kind === "superseded") {
						reportReadFailures(reading);
						return;
					}
					observing.visibility = {
						kind: reading.isActive ? "visible" : "hidden",
					};
					state.setValue(getIndicatorSnapshot(observing.visibility));
					reportReadFailures(reading);
				},
				target === "visible" ? showDelayMillis : hideDelayMillis,
			),
		};
		observing.visibility = scheduled;
	}

	function reconcileActivity(): void {
		if (lifecycle.kind === "disposed") {
			return;
		}
		const observing = lifecycle;
		const reading = readActivity(observing);
		if (reading.kind === "superseded") {
			reportReadFailures(reading);
			return;
		}
		const previous = observing.visibility;
		switch (previous.kind) {
			case "hidden":
				if (reading.isActive) {
					scheduleVisibility(observing, "visible");
				}
				break;
			case "showing":
				if (!reading.isActive) {
					clearVisibilityTimer(previous);
					observing.visibility = { kind: "hidden" };
				}
				break;
			case "visible":
				if (!reading.isActive) {
					scheduleVisibility(observing, "hidden");
				}
				break;
			case "hiding":
				if (reading.isActive) {
					clearVisibilityTimer(previous);
					observing.visibility = { kind: "visible" };
				}
				break;
			case "show-unconfirmed":
			case "hide-unconfirmed":
				observing.visibility = {
					kind: reading.isActive ? "visible" : "hidden",
				};
				break;
		}
		if (observing.visibility !== previous) {
			state.setValue(getIndicatorSnapshot(observing.visibility));
		}
		reportReadFailures(reading);
	}

	function dispose(): void {
		if (lifecycle.kind === "disposed") {
			return;
		}
		const observing = lifecycle;
		lifecycle = { kind: "disposed" };
		clearVisibilityTimer(observing.visibility);
		const disposed = callWithResult(() => observing.scope.dispose());
		observing.sources.length = 0;
		state.setValue(getIndicatorSnapshot({ kind: "hidden" }));
		if (disposed.kind === "error") {
			throw disposed.error;
		}
	}

	const observing = lifecycle;
	const subscribed = callWithResult(() => {
		for (const source of observing.sources) {
			observing.scope.addCleanup(source.subscribe(reconcileActivity));
		}
		reconcileActivity();
	});
	if (subscribed.kind === "error") {
		const disposed = callWithResult(dispose);
		if (disposed.kind === "error") {
			throw new AggregateError(
				[subscribed.error, disposed.error],
				"Activity indicator setup and cleanup failed.",
			);
		}
		throw subscribed.error;
	}

	return createSuccessResult({
		getState: state.getValue,
		subscribe: (listener) => {
			if (lifecycle.kind === "disposed") {
				return () => undefined;
			}
			const unsubscribe = state.subscribe(listener);
			const detach = lifecycle.scope.addCleanup(unsubscribe);
			return () => {
				detach();
				unsubscribe();
			};
		},
		dispose,
	});
}
