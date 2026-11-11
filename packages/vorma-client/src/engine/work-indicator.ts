import {
	createActivityIndicator,
	createActivityTracker,
	type ActivityIndicatorConfigError,
	type ActivitySource,
} from "primitives-ts/async-work/activity-indicator";
import { createSuccessResult, type Result } from "primitives-ts/results";
import type { CoreState } from "./core-state.ts";
import type { ClientCtx } from "./ctx.ts";
import { reportFailure } from "./events.ts";
import { patchState } from "./state.ts";

// Application work can begin before a client exists and outlive that client.
const applicationActivity = createActivityTracker({
	onListenerErrorFn: (cause) => {
		console.error("A Vorma application-activity listener failed.", cause);
	},
});

/** Includes the promise's work until settlement and returns the same promise. */
export function includeInWorkIndicator<Value>(
	promise: Promise<Value>,
): Promise<Value> {
	return applicationActivity.trackPromise(promise);
}

/**
 * Whether the work in the state counts toward the indicator: navigations,
 * revalidations, and endpoint calls that do not skip it. Prefetches never
 * count, because a prefetch is speculative background work that the user did
 * not ask to wait for.
 */
function checkIncludedWork(ctx: ClientCtx, state: CoreState): boolean {
	const options = ctx.options.workIndicator;
	if (options === undefined) {
		return false;
	}
	return (
		(options.skipNavs !== true &&
			state.nav !== undefined &&
			!state.nav.skipWorkIndicator) ||
		(options.skipRevalidations !== true &&
			state.revalidation.phase !== undefined &&
			!state.revalidation.skipWorkIndicator) ||
		(options.skipApiEndpointCalls !== true &&
			state.apiEndpointCalls.some((call) => !call.skipWorkIndicator))
	);
}

/** Starts the indicator for a client and returns its cleanup operation. */
export function startWorkIndicator(
	ctx: ClientCtx,
): Result<() => void, ActivityIndicatorConfigError> {
	const options = ctx.options.workIndicator;
	if (options === undefined) {
		return createSuccessResult(() => undefined);
	}
	function reportListenerError(cause: unknown): void {
		reportFailure(ctx, "listener", cause);
	}
	const frameworkActivity: ActivitySource = {
		getActiveCount: () =>
			checkIncludedWork(ctx, ctx.state.getValue()) ? 1 : 0,
		subscribe: (listener) => {
			return ctx.state.subscribe(({ value, previousValue }) => {
				listener({
					value: checkIncludedWork(ctx, value) ? 1 : 0,
					previousValue: checkIncludedWork(ctx, previousValue)
						? 1
						: 0,
				});
			});
		},
	};
	const created = createActivityIndicator({
		sources: [applicationActivity, frameworkActivity],
		showDelayMillis: options.startDelayMillis,
		hideDelayMillis: options.stopDelayMillis,
		onListenerErrorFn: reportListenerError,
	});
	if (created.kind === "error") {
		return created;
	}
	const indicator = created.value;
	indicator.subscribe(({ value }) => {
		if (ctx.state.getValue().workIndicatorVisible !== value.isVisible) {
			patchState(ctx, { workIndicatorVisible: value.isVisible });
		}
	});
	return createSuccessResult(indicator.dispose);
}
