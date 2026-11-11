import type { RevalidationResult } from "../results.ts";
import type {
	AttemptExecutor,
	FreshnessAttempt,
	FreshnessDemand,
	FreshnessFacts,
	Waiter,
} from "./ctx.ts";

/*
A freshness demand requires a newer committed page and acknowledgements from
its expected watched queries. A running attempt owns its waiter batch and names
its executor: a revalidation candidate, or the navigation that holds the slot.
Failure settles that batch and suspends the demand until another authorized
trigger renews it. These pure transitions return completions for execution
after their new state is installed.
*/

export type FreshnessTransition = {
	readonly facts: FreshnessFacts;
	readonly completions: ReadonlyArray<() => void>;
	readonly shouldStartAttempt: boolean;
};
const IDLE_ATTEMPT: FreshnessAttempt = { phase: "idle" };
export const INITIAL_FRESHNESS: FreshnessFacts = {
	epoch: 0,
	state: { kind: "idle" },
};

export function getFreshnessDemand(
	facts: FreshnessFacts,
): FreshnessDemand | null {
	return facts.state.kind === "outstanding" ? facts.state.demand : null;
}

export function getFreshnessAttempt(facts: FreshnessFacts): FreshnessAttempt {
	return facts.state.kind === "outstanding"
		? facts.state.attempt
		: IDLE_ATTEMPT;
}

function getAttemptWaiters(facts: FreshnessFacts): ReadonlyArray<Waiter> {
	const attempt = getFreshnessAttempt(facts);
	return attempt.phase === "running" ? attempt.waiters : [];
}

function settleWaiters(
	waiters: ReadonlyArray<Waiter>,
	result: RevalidationResult,
): ReadonlyArray<() => void> {
	return waiters.map((waiter) => () => waiter(result));
}

/** Registers a demand and resets query expectations for its new epoch. */
export function registerDemand(
	facts: FreshnessFacts,
	input: {
		readonly sequence: number;
		readonly waiter: Waiter | undefined;
		readonly skipWorkIndicator: boolean;
	},
): FreshnessTransition {
	const attempt = getFreshnessAttempt(facts);
	const previous = getFreshnessDemand(facts);
	const demand: FreshnessDemand = {
		afterSequence: input.sequence,
		skipWorkIndicator:
			(previous?.skipWorkIndicator ?? input.skipWorkIndicator) &&
			input.skipWorkIndicator,
		expectedQueries: new Set(),
		acknowledgedQueries: new Map(),
		page: { kind: "awaiting" },
		waiters:
			input.waiter === undefined
				? (previous?.waiters ?? [])
				: [...(previous?.waiters ?? []), input.waiter],
	};
	return {
		facts: {
			epoch: facts.epoch + 1,
			state: {
				kind: "outstanding",
				demand,
				attempt:
					attempt.phase === "running"
						? { ...attempt, renewal: "renewed" }
						: IDLE_ATTEMPT,
			},
		},
		completions: [],
		shouldStartAttempt: attempt.phase !== "running",
	};
}

/** Transfers the demand's current waiter batch into a running attempt that the executor carries. */
export function startAttempt(
	facts: FreshnessFacts,
	executor: AttemptExecutor,
): FreshnessFacts {
	const state = facts.state;
	if (state.kind === "idle" || state.attempt.phase === "running") {
		return facts;
	}
	return {
		...facts,
		state: {
			kind: "outstanding",
			demand: { ...state.demand, waiters: [] },
			attempt: {
				phase: "running",
				executor,
				coveredSequence: state.demand.afterSequence,
				renewal: "original",
				waiters: state.demand.waiters,
			},
		},
	};
}

/** Hands a running attempt to the candidate that takes the slot, which then executes it. */
export function transferAttempt(
	facts: FreshnessFacts,
	executor: AttemptExecutor,
): FreshnessFacts {
	const state = facts.state;
	if (
		state.kind === "idle" ||
		state.attempt.phase !== "running" ||
		state.attempt.executor === executor
	) {
		return facts;
	}
	return {
		...facts,
		state: { ...state, attempt: { ...state.attempt, executor } },
	};
}

function completeIfSatisfied(
	facts: FreshnessFacts,
	demand: FreshnessDemand,
): FreshnessTransition {
	if (facts.state.kind === "idle") {
		return { facts, completions: [], shouldStartAttempt: false };
	}
	const outstanding = [...demand.expectedQueries].some((id) => {
		const acknowledged = demand.acknowledgedQueries.get(id);
		return (
			acknowledged === undefined || acknowledged <= demand.afterSequence
		);
	});
	if (demand.page.kind === "awaiting" || outstanding) {
		return {
			facts: { ...facts, state: { ...facts.state, demand } },
			completions: [],
			shouldStartAttempt: false,
		};
	}
	return {
		facts: { ...facts, state: { kind: "idle" } },
		completions: settleWaiters(
			[...getAttemptWaiters(facts), ...demand.waiters],
			{ kind: "revalidated" },
		),
		shouldStartAttempt: false,
	};
}

/** Observes a page that committed without an error. */
export function satisfyDemand(
	facts: FreshnessFacts,
	input: { readonly sequence: number },
): FreshnessTransition {
	const state = facts.state;
	if (state.kind === "idle") {
		return { facts, completions: [], shouldStartAttempt: false };
	}
	const { demand, attempt } = state;
	if (input.sequence <= demand.afterSequence) {
		if (attempt.phase === "failed") {
			return { facts, completions: [], shouldStartAttempt: false };
		}
		return {
			facts: {
				...facts,
				state: {
					kind: "outstanding",
					demand: {
						...demand,
						waiters: [
							...getAttemptWaiters(facts),
							...demand.waiters,
						],
					},
					attempt: IDLE_ATTEMPT,
				},
			},
			completions: [],
			shouldStartAttempt: true,
		};
	}
	return completeIfSatisfied(facts, {
		...demand,
		page: { kind: "read", sequence: input.sequence },
	});
}

/** A qualifying query failure suspends the demand; a fresh answer acknowledges it. */
export function acknowledgeQuery(
	facts: FreshnessFacts,
	input: {
		readonly id: string;
		readonly sequence: number;
		readonly success: boolean;
		readonly cause: unknown;
	},
): FreshnessTransition {
	const state = facts.state;
	if (
		state.kind === "idle" ||
		!state.demand.expectedQueries.has(input.id) ||
		input.sequence <= state.demand.afterSequence
	) {
		return { facts, completions: [], shouldStartAttempt: false };
	}
	const { demand } = state;
	if (!input.success) {
		return {
			facts: {
				...facts,
				state: {
					kind: "outstanding",
					demand: { ...demand, waiters: [] },
					attempt: {
						phase: "failed",
						coveredSequence: demand.afterSequence,
					},
				},
			},
			completions: settleWaiters(
				[...getAttemptWaiters(facts), ...demand.waiters],
				{ kind: "failure", reason: "query", cause: input.cause },
			),
			shouldStartAttempt: false,
		};
	}
	return completeIfSatisfied(facts, {
		...demand,
		acknowledgedQueries: new Map([
			...demand.acknowledgedQueries,
			[input.id, input.sequence],
		]),
	});
}

/** Adds the watched queries that must acknowledge this epoch. */
export function expectQueries(
	facts: FreshnessFacts,
	ids: ReadonlySet<string>,
): FreshnessFacts {
	const state = facts.state;
	if (state.kind === "idle" || ids.size === 0) {
		return facts;
	}
	return {
		...facts,
		state: {
			...state,
			demand: {
				...state.demand,
				expectedQueries: new Set([
					...state.demand.expectedQueries,
					...ids,
				]),
			},
		},
	};
}

/** Releases a query's acknowledgement obligation when watching stops. */
export function releaseQuery(
	facts: FreshnessFacts,
	id: string,
): FreshnessTransition {
	const state = facts.state;
	if (state.kind === "idle" || !state.demand.expectedQueries.has(id)) {
		return { facts, completions: [], shouldStartAttempt: false };
	}
	const expectedQueries = new Set(state.demand.expectedQueries);
	expectedQueries.delete(id);
	return completeIfSatisfied(facts, { ...state.demand, expectedQueries });
}

/** Settles the running batch and permits another attempt only for a renewed demand. */
export function failAttempt(
	facts: FreshnessFacts,
	result: RevalidationResult,
): FreshnessTransition {
	const state = facts.state;
	if (state.kind === "idle") {
		return { facts, completions: [], shouldStartAttempt: false };
	}
	const { demand, attempt } = state;
	const coveredSequence =
		attempt.phase === "idle"
			? demand.afterSequence
			: attempt.coveredSequence;
	const again =
		(attempt.phase === "running" && attempt.renewal === "renewed") ||
		demand.afterSequence > coveredSequence;
	return {
		facts: {
			...facts,
			state: {
				...state,
				attempt: again
					? IDLE_ATTEMPT
					: { phase: "failed", coveredSequence },
			},
		},
		completions: settleWaiters(getAttemptWaiters(facts), result),
		shouldStartAttempt: again,
	};
}

/**
 * Returns the running batch to its demand when its candidate cannot settle it.
 * The next attempt starts when the caller finds the slot free and the browser
 * on the page that a revalidation must read.
 */
export function abandonAttempt(facts: FreshnessFacts): FreshnessTransition {
	const state = facts.state;
	if (state.kind === "idle" || state.attempt.phase !== "running") {
		return { facts, completions: [], shouldStartAttempt: false };
	}
	return {
		facts: {
			...facts,
			state: {
				kind: "outstanding",
				demand: {
					...state.demand,
					waiters: [
						...getAttemptWaiters(facts),
						...state.demand.waiters,
					],
				},
				attempt: IDLE_ATTEMPT,
			},
		},
		completions: [],
		shouldStartAttempt: false,
	};
}

export function checkStartableAttempt(facts: FreshnessFacts): boolean {
	const state = facts.state;
	return (
		state.kind === "outstanding" &&
		state.attempt.phase !== "running" &&
		(state.attempt.phase !== "failed" ||
			state.demand.afterSequence > state.attempt.coveredSequence)
	);
}

/** Retires a demand and settles every pending observation with its terminal result. */
export function retireDemand(
	facts: FreshnessFacts,
	result: RevalidationResult,
): FreshnessTransition {
	return {
		facts: { ...facts, state: { kind: "idle" } },
		completions: settleWaiters(
			[
				...getAttemptWaiters(facts),
				...(getFreshnessDemand(facts)?.waiters ?? []),
			],
			result,
		),
		shouldStartAttempt: false,
	};
}
