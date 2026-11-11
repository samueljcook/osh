import { awaitWithAbort } from "primitives-ts/async-work/cancellation";
import { callWithResultAsync, type Result } from "primitives-ts/results";
import type { CommitReason, PageState } from "../results.ts";
import {
	CancelPageCommit,
	type BeforePageCommitCtx,
	type RendererNeutralViewDef,
} from "../views.ts";
import type { CommitFailure } from "./commit.ts";
import type { Frame } from "./core-state.ts";
import type { Candidate, ClientCtx } from "./ctx.ts";
import type { PreparedPage } from "./prepare.ts";

/*
The step between a prepared page and its commit. Every view in the current page
or the next page runs its `beforePageCommit` hook once, concurrently. Then the
application's `commitPageFn`, if any, decides how the commit happens or
declines it. A deliberate cancel is not a failure.
*/

type PageCommitDecision =
	| { readonly kind: "committed"; readonly frame: Frame }
	| { readonly kind: "cancelled" }
	| { readonly kind: "superseded" }
	| {
			readonly kind: "failure";
			readonly stage: "commit" | "document";
			readonly cause: unknown;
	  };

/** The page that a commit would produce, for hooks and `commitPageFn`. */
export function previewPage(input: {
	readonly prepared: PreparedPage;
	readonly href: string;
	readonly historyState: unknown;
	readonly reason: CommitReason;
}): PageState {
	return {
		href: input.href,
		historyState: input.historyState,
		matches: input.prepared.entries.map((entry) => entry.match),
		error: input.prepared.error?.error,
		commitReason: input.reason,
	};
}

type HookTarget = {
	readonly view: RendererNeutralViewDef;
	readonly presence: "leaving" | "arriving" | "staying";
};

function selectHookTargets(
	ctx: ClientCtx,
	prepared: PreparedPage,
): ReadonlyArray<HookTarget> {
	const current = ctx.state.getValue().frame?.entries ?? [];
	const next = prepared.entries;
	const targets: Array<HookTarget> = [];
	for (const [index, entry] of current.entries()) {
		if (entry.view?.beforePageCommit === undefined) {
			continue;
		}
		const staying = next[index]?.pattern === entry.pattern;
		targets.push({
			view: entry.view,
			presence: staying ? "staying" : "leaving",
		});
	}
	for (const [index, entry] of next.entries()) {
		if (entry.view?.beforePageCommit === undefined) {
			continue;
		}
		if (current[index]?.pattern === entry.pattern) {
			continue;
		}
		targets.push({ view: entry.view, presence: "arriving" });
	}
	return targets;
}

type HookDecision =
	| { readonly kind: "proceed" }
	| { readonly kind: "cancel" }
	| { readonly kind: "failure"; readonly cause: unknown };

function runHooks(
	ctx: ClientCtx,
	input: {
		readonly candidate: Candidate;
		readonly prepared: PreparedPage;
		readonly reason: BeforePageCommitCtx["reason"];
		readonly current: PageState;
		readonly next: PageState;
	},
): Promise<HookDecision> {
	const targets = selectHookTargets(ctx, input.prepared);
	if (targets.length === 0) {
		return Promise.resolve({ kind: "proceed" });
	}
	const hookScope = input.candidate.scope.createChild();
	return new Promise((resolve) => {
		let state:
			| { readonly kind: "running"; readonly remaining: number }
			| { readonly kind: "decided" } = {
			kind: "running",
			remaining: targets.length,
		};
		function decide(decision: HookDecision): void {
			if (state.kind === "decided") {
				return;
			}
			state = { kind: "decided" };
			if (decision.kind !== "proceed") {
				// The other hooks' signals abort.
				hookScope.dispose();
			}
			resolve(decision);
		}
		for (const { view, presence } of targets) {
			const hook = view.beforePageCommit;
			void callWithResultAsync(() => {
				return hook?.({
					reason: input.reason,
					presence,
					current: input.current,
					next: input.next,
					signal: hookScope.signal,
				});
			}).then((result) => {
				if (state.kind === "decided") {
					return;
				}
				if (result.kind === "error") {
					decide({ kind: "failure", cause: result.error });
					return;
				}
				if (result.value === CancelPageCommit) {
					decide({ kind: "cancel" });
					return;
				}
				state = { kind: "running", remaining: state.remaining - 1 };
				if (state.remaining === 0) {
					decide({ kind: "proceed" });
				}
			});
		}
	});
}

/**
 * Runs the hooks and `commitPageFn`, then commits. `commitNowFn` is the
 * synchronous commit section: it returns undefined when the candidate has been
 * superseded and then does nothing.
 */
export async function runPageCommit(
	ctx: ClientCtx,
	input: {
		readonly candidate: Candidate;
		readonly prepared: PreparedPage;
		readonly reason: BeforePageCommitCtx["reason"];
		readonly next: PageState;
		readonly commitNowFn: () => Result<Frame, CommitFailure> | undefined;
	},
): Promise<PageCommitDecision> {
	const { candidate } = input;
	const currentPage = ctx.publicState.getValue()?.page;
	if (currentPage !== undefined) {
		const waiting = await awaitWithAbort({
			signal: candidate.scope.signal,
			operationFn: () => {
				return runHooks(ctx, {
					candidate,
					prepared: input.prepared,
					reason: input.reason,
					current: currentPage,
					next: input.next,
				});
			},
		});
		if (candidate.scope.signal.aborted) {
			return { kind: "superseded" };
		}
		if (waiting.kind === "error") {
			return waiting.error.kind === "aborted"
				? { kind: "superseded" }
				: {
						kind: "failure",
						stage: "commit",
						cause: waiting.error.cause,
					};
		}
		const hooks = waiting.value;
		if (hooks.kind === "cancel") {
			return { kind: "cancelled" };
		}
		if (hooks.kind === "failure") {
			return { kind: "failure", stage: "commit", cause: hooks.cause };
		}
	}
	function toDecision(
		committed: Result<Frame, CommitFailure> | undefined,
	): PageCommitDecision {
		if (committed === undefined) {
			return { kind: "superseded" };
		}
		return committed.kind === "ok"
			? { kind: "committed", frame: committed.value }
			: { kind: "failure", ...committed.error };
	}
	const commitPageFn = ctx.options.commitPageFn;
	if (commitPageFn === undefined || currentPage === undefined) {
		return toDecision(input.commitNowFn());
	}
	type CommitState =
		| { readonly kind: "waiting" }
		| { readonly kind: "committing" }
		| { readonly kind: "decided"; readonly decision: PageCommitDecision };
	let state: CommitState = { kind: "waiting" };
	const committed = Promise.withResolvers<void>();
	function commit(): void {
		if (state.kind !== "waiting" || candidate.scope.signal.aborted) {
			return;
		}
		state = { kind: "committing" };
		state = { kind: "decided", decision: toDecision(input.commitNowFn()) };
		committed.resolve();
	}
	function finish(decision: PageCommitDecision): PageCommitDecision {
		if (state.kind === "decided") {
			return state.decision;
		}
		state = { kind: "decided", decision };
		return decision;
	}
	const running = callWithResultAsync(() => {
		return commitPageFn({
			reason: input.reason,
			current: currentPage,
			next: input.next,
			commit,
			signal: candidate.scope.signal,
		});
	});
	// The operation is committed as soon as `commit()` has run, whatever the
	// returned promise does afterward, such as waiting for an animation to end.
	const settled = await awaitWithAbort({
		signal: candidate.scope.signal,
		operationFn: () =>
			Promise.race([running, committed.promise.then(() => undefined)]),
	});
	// The accessor reads mutations that an application callback can make while awaiting.
	function getDecision(): PageCommitDecision | undefined {
		return state.kind === "decided" ? state.decision : undefined;
	}
	const decision = getDecision();
	if (decision !== undefined) {
		return decision;
	}
	if (candidate.scope.signal.aborted) {
		return finish({ kind: "superseded" });
	}
	if (settled.kind === "error") {
		return finish(
			settled.error.kind === "aborted"
				? { kind: "superseded" }
				: {
						kind: "failure",
						stage: "commit",
						cause: settled.error.cause,
					},
		);
	}
	const returned = await running;
	if (returned.kind === "error") {
		return finish({
			kind: "failure",
			stage: "commit",
			cause: returned.error,
		});
	}
	if (returned.value === CancelPageCommit) {
		return finish({ kind: "cancelled" });
	}
	return finish({
		kind: "failure",
		stage: "commit",
		cause: new Error(
			"commitPageFn settled without calling commit() or returning CancelPageCommit. When commit() runs later, such as in the callback of a view transition, commitPageFn must return a promise that settles after commit() has run.",
		),
	});
}
