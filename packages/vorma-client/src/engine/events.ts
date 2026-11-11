import { callWithResult } from "primitives-ts/results";
import type { ClientEvent, FailureSource } from "../results.ts";
import type { ClientCtx } from "./ctx.ts";

/** Delivers an event to every listener; a throwing listener is reported and never reaches the others. */
export function publishEvent(ctx: ClientCtx, event: ClientEvent): void {
	if (ctx.state.getValue().lifecycle === "disposed") {
		return;
	}
	for (const listener of Array.from(ctx.eventListeners)) {
		const delivered = callWithResult(() => listener(event));
		if (delivered.kind === "ok") {
			continue;
		}
		if (event.kind === "failure-reported") {
			console.error(
				"A Vorma failure listener failed while handling a reported failure.",
				delivered.error,
			);
		} else {
			reportFailure(ctx, "listener", delivered.error);
		}
	}
}

/**
 * Reports a failure that Vorma handled itself. The report goes to
 * `console.error` and to the event subscribers, which can forward it to error
 * tracking without knowing its source.
 */
export function reportFailure(
	ctx: ClientCtx,
	source: FailureSource,
	cause: unknown,
): void {
	console.error(`Vorma reported a failure (${source}).`, cause);
	publishEvent(ctx, { kind: "failure-reported", source, cause });
}

export function subscribeToEvents(
	ctx: ClientCtx,
	listener: (event: ClientEvent) => void,
): () => void {
	ctx.eventListeners.add(listener);
	return () => {
		ctx.eventListeners.delete(listener);
	};
}
