import { Effect, Queue, Stream } from "effect";
import { normalizePath } from "vite";

/**
 * Batches native watcher events after 30 ms without another event. Completed
 * batches accumulate while the consumer is busy; no changed path is discarded,
 * and `take` waits for at least one changed path. A watcher error is logged,
 * and watching continues.
 */
export function createDevChanges() {
	return Effect.gen(function* () {
		const notifications = yield* Queue.make<string>();
		const errors = yield* Queue.make<unknown>();
		const batches = yield* Queue.unbounded<ReadonlySet<string>>();
		let incoming = new Set<string>();
		function recordChange(filePath: string) {
			Queue.offerUnsafe(notifications, normalizePath(filePath));
		}
		function recordError(cause: unknown) {
			Queue.offerUnsafe(errors, cause);
		}
		yield* Stream.fromQueue(errors).pipe(
			Stream.runForEach((cause) => {
				return Effect.logError(
					"The file watcher reported an error, and Vorma keeps watching",
					cause,
				);
			}),
			Effect.forkScoped,
		);
		yield* Stream.fromQueue(notifications).pipe(
			Stream.tap((filePath) => Effect.sync(() => incoming.add(filePath))),
			Stream.debounce("30 millis"),
			Stream.runForEach(() => {
				return Effect.sync(() => {
					// An earlier flush can have taken the paths of this emission.
					if (incoming.size === 0) {
						return;
					}
					const batch = incoming;
					incoming = new Set();
					Queue.offerUnsafe(batches, batch);
				});
			}),
			Effect.forkScoped,
		);
		const take = Effect.gen(function* () {
			const collected = new Set<string>();
			for (const batch of yield* Queue.takeAll(batches)) {
				for (const filePath of batch) {
					collected.add(filePath);
				}
			}
			return collected as ReadonlySet<string>;
		});
		return {
			take,
			recordChange,
			recordError,
		};
	});
}
