import { createDebouncedFunction } from "primitives-ts/async-work/debounce";
import { subscribeToWindowFocus } from "primitives-ts/browser/focus";
import { subscribeToDocumentVisibility } from "primitives-ts/browser/visibility";
import {
	createErrorResult,
	createSuccessResult,
	type Result,
} from "primitives-ts/results";
import type { ClientState, RevalidationResult } from "../results.ts";
import { checkNavIdle, getActiveCandidate } from "./candidate-slot.ts";
import type { ClientCtx, FreshnessFacts, Waiter } from "./ctx.ts";
import { reportFailure } from "./events.ts";
import {
	checkStartableAttempt,
	getFreshnessAttempt,
	getFreshnessDemand,
	expectQueries,
	registerDemand,
	startAttempt,
	type FreshnessTransition,
} from "./freshness.ts";
import { startRevalidationCandidate } from "./nav.ts";
import { retirePreparationsAndOutputs } from "./prefetch.ts";
import { updateState } from "./state.ts";

/*
Revalidation: demands for fresh data and the attempts that satisfy them.
Manual and focus demands start 8 ms after the last request, so that demands in
one turn share one read; a mutation's demand starts at once. Vorma never
retries on its own: a failed attempt keeps its demand, and the next trigger
reads again.
*/

const REQUEST_DEBOUNCE_MILLIS = 8;

const recordedResults = new WeakSet<RevalidationResult>();

/** Derives the public revalidation state slice from the freshness facts, keeping identity when unchanged. */
export function deriveRevalidationSlice(input: {
	readonly facts: FreshnessFacts;
	readonly current: ClientState["revalidation"];
}): ClientState["revalidation"] {
	const { facts, current } = input;
	const demand = getFreshnessDemand(facts);
	const attempt = getFreshnessAttempt(facts);
	const phase: "scheduled" | "running" | undefined =
		attempt.phase === "running"
			? "running"
			: demand !== null && checkStartableAttempt(facts)
				? "scheduled"
				: undefined;
	const required = demand !== null;
	const skipWorkIndicator = demand?.skipWorkIndicator ?? false;
	if (
		current.required === required &&
		current.phase === phase &&
		current.skipWorkIndicator === skipWorkIndicator
	) {
		return current;
	}
	return {
		...current,
		required,
		phase,
		skipWorkIndicator,
	};
}

/** Writes the public revalidation state from the freshness facts. */
function syncRevalidationState(ctx: ClientCtx): void {
	const current = ctx.state.getValue().revalidation;
	const next = deriveRevalidationSlice({ facts: ctx.freshness, current });
	if (next === current) {
		return;
	}
	updateState(ctx, (state) => {
		return {
			...state,
			revalidation: deriveRevalidationSlice({
				facts: ctx.freshness,
				current: state.revalidation,
			}),
		};
	});
}

function recordLastResult(ctx: ClientCtx, result: RevalidationResult): void {
	updateState(ctx, (state) => {
		return {
			...state,
			revalidation: { ...state.revalidation, lastResult: result },
		};
	});
}

/** Stores the new facts, runs the completions, and starts an attempt when the transition asks for one. */
export function applyFreshness(
	ctx: ClientCtx,
	transition: FreshnessTransition,
): void {
	ctx.freshness = transition.facts;
	syncRevalidationState(ctx);
	for (const completion of transition.completions) {
		completion();
	}
	if (transition.shouldStartAttempt) {
		startAttemptNow(ctx);
	}
}

export function cancelScheduledRevalidation(ctx: ClientCtx): void {
	if (ctx.revalidationScheduling.kind === "ready") {
		ctx.revalidationScheduling.debounce.cancelPendingCalls(undefined);
	}
}

/**
 * Registers a demand for fresh data. Every registration first retires the
 * reusable data. The waiter receives the result of the attempt that satisfies
 * the demand.
 */
export function registerRevalidationDemand(
	ctx: ClientCtx,
	input: {
		readonly immediate: boolean;
		readonly skipWorkIndicator: boolean;
		readonly waiter: Waiter | undefined;
		/**
		 * The sequence after which a page read satisfies the demand. A demand
		 * registers a new sequence when this is absent. A mutation's redirect
		 * passes the sequence from before its own navigation read, so that the
		 * redirect's read satisfies the demand.
		 */
		readonly afterSequence?: number;
	},
): void {
	if (ctx.state.getValue().lifecycle !== "ready") {
		input.waiter?.({ kind: "client-disposed" });
		return;
	}
	retirePreparationsAndOutputs(ctx);
	function waiter(result: RevalidationResult): void {
		// Every waiter of a batch receives the same result object.
		if (!recordedResults.has(result)) {
			recordedResults.add(result);
			recordLastResult(ctx, result);
			if (result.kind === "failure") {
				reportFailure(ctx, "revalidation", result.cause);
			}
		}
		input.waiter?.(result);
	}
	const transition = registerDemand(ctx.freshness, {
		sequence: input.afterSequence ?? ++ctx.lastSequence,
		waiter,
		skipWorkIndicator: input.skipWorkIndicator,
	});
	ctx.freshness = transition.facts;
	// Watched queries read again in the same step that registers the demand.
	const expected = new Set<string>();
	for (const query of ctx.watchedQueries) {
		if (query.fetchForDemand()) {
			expected.add(query.id);
		}
	}
	ctx.freshness = expectQueries(ctx.freshness, expected);
	cancelScheduledRevalidation(ctx);
	if (!transition.shouldStartAttempt) {
		syncRevalidationState(ctx);
		return;
	}
	if (input.immediate) {
		syncRevalidationState(ctx);
		startAttemptNow(ctx);
		return;
	}
	if (ctx.revalidationScheduling.kind === "uninitialized") {
		const created = createDebouncedFunction({
			delayMillis: REQUEST_DEBOUNCE_MILLIS,
			operationFn: () => startAttemptNow(ctx),
		});
		if (created.kind === "error") {
			throw new Error(
				"The fixed revalidation delay must fit the supported timer range.",
			);
		}
		ctx.revalidationScheduling = { kind: "ready", debounce: created.value };
	}
	void ctx.revalidationScheduling.debounce().then((result) => {
		if (result.kind === "error" && result.error.kind === "failed") {
			reportFailure(ctx, "revalidation", result.error.cause);
		}
	});
	syncRevalidationState(ctx);
}

/** Starts the attempt for the outstanding demand when nothing else blocks it. */
function startAttemptNow(ctx: ClientCtx): void {
	if (
		(ctx.revalidationScheduling.kind === "ready" &&
			ctx.revalidationScheduling.debounce.checkPendingCalls()) ||
		ctx.state.getValue().lifecycle !== "ready" ||
		ctx.state.getValue().frame === undefined ||
		ctx.navMachine.kind === "returning-history" ||
		ctx.navMachine.kind === "disposed" ||
		!checkStartableAttempt(ctx.freshness)
	) {
		return;
	}
	const demand = getFreshnessDemand(ctx.freshness);
	if (demand === null) {
		return;
	}
	const active = getActiveCandidate(ctx);
	ctx.freshness = startAttempt(
		ctx.freshness,
		active === undefined || active.role.kind === "revalidation"
			? "revalidation"
			: "navigation",
	);
	syncRevalidationState(ctx);
	if (active !== undefined) {
		// The candidate in the slot settles the attempt with its commit, or
		// returns the attempt to its demand.
		return;
	}
	startRevalidationCandidate(ctx, {
		skipWorkIndicator: demand.skipWorkIndicator,
	});
}

/** Starts an attempt for an outstanding demand unless history restoration or disposal blocks it. */
export function maybeStartAttempt(ctx: ClientCtx): void {
	if (ctx.navMachine.kind === "idle" || ctx.navMachine.kind === "running") {
		startAttemptNow(ctx);
	}
}

/**
 * Checks the window-focus option before boot attaches it: its stale time must
 * be a finite number of milliseconds that is zero or more.
 */
export function checkRevalidationOptions(ctx: ClientCtx): Result<void, Error> {
	const options = ctx.options.revalidateOnWindowFocus;
	if (
		options === undefined ||
		(Number.isFinite(options.staleTimeMillis) &&
			options.staleTimeMillis >= 0)
	) {
		return createSuccessResult(undefined);
	}
	return createErrorResult(
		new Error(
			`revalidateOnWindowFocus.staleTimeMillis must be a finite number of milliseconds that is zero or more, not ${String(options.staleTimeMillis)}.`,
		),
	);
}

/** Attaches the window focus trigger. */
export function attachRevalidationTriggers(ctx: ClientCtx): () => void {
	const options = ctx.options.revalidateOnWindowFocus;
	if (options === undefined) {
		return () => undefined;
	}
	function handleFocus(): void {
		const frame = ctx.state.getValue().frame;
		if (
			frame === undefined ||
			ctx.state.getValue().lifecycle !== "ready" ||
			!checkNavIdle(ctx) ||
			getFreshnessAttempt(ctx.freshness).phase === "running" ||
			// A mutation in flight may still change the page, whether or not it
			// revalidates afterward.
			ctx.state
				.getValue()
				.apiEndpointCalls.some((call) => call.kind === "mutation") ||
			options === undefined ||
			Date.now() - frame.preparedAt < options.staleTimeMillis
		) {
			return;
		}
		registerRevalidationDemand(ctx, {
			immediate: false,
			skipWorkIndicator: options.skipWorkIndicator === true,
			waiter: undefined,
		});
	}
	const focus = subscribeToWindowFocus({
		window,
		timing: { kind: "immediate" },
		listener: handleFocus,
		onListenerErrorFn: (cause) => reportFailure(ctx, "listener", cause),
	});
	const visibility = subscribeToDocumentVisibility({
		document,
		timing: { kind: "immediate" },
		listener: (visibilityState) => {
			if (visibilityState === "visible") {
				handleFocus();
			}
		},
		onListenerErrorFn: (cause) => reportFailure(ctx, "listener", cause),
	});
	if (focus.kind === "error" || visibility.kind === "error") {
		throw new Error(
			"Immediate browser subscriptions cannot have an invalid debounce delay.",
		);
	}
	return () => {
		focus.value();
		visibility.value();
	};
}
