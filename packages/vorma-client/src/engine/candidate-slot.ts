import type { Candidate, ClientCtx } from "./ctx.ts";

/** Projects the candidate that owns the running navigation state. */
export function getActiveCandidate(ctx: ClientCtx): Candidate | undefined {
	const machine = ctx.navMachine;
	return machine.kind === "running" ? machine.candidate : undefined;
}

/** Checks whether the navigation machine is idle and ready to start revalidation work. */
export function checkNavIdle(ctx: ClientCtx): boolean {
	return ctx.navMachine.kind === "idle";
}
