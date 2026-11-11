import {
	type Cause,
	Deferred,
	Effect,
	Exit,
	Fiber,
	Queue,
	Scope,
} from "effect";
import type { ResolvedDevServer } from "../config/resolve-config.ts";
import type { DevPortIssue } from "./ports.ts";
import { startDevServerProcess } from "./server-process.ts";

/*
The application's own server process during one development session. The
session starts and stops it; an exit that the session did not request is
reported through `unexpectedExit`. The process belongs to the session's scope.
Whenever no process runs, `vorma dev` holds the application's port, so that a
concurrent `vorma dev` process cannot take it: a stop holds the port again
after the process has ended, and so does the end of a process that the session
did not stop.
*/

/** Vorma's reservation of the application's port, which the development command keeps across sessions. */
export type DevApplicationPort = {
	readonly port: number;
	/** Reserves the port again, unless Vorma holds it. */
	readonly hold: Effect.Effect<void, DevPortIssue>;
	/** Ends Vorma's reservation, just before the application binds the port. */
	readonly release: Effect.Effect<void>;
};

function testCmd(value: unknown): value is readonly [string, ...Array<string>] {
	return (
		Array.isArray(value) &&
		typeof value[0] === "string" &&
		value[0] !== "" &&
		value.every((part) => typeof part === "string")
	);
}

/**
 * The command that runs the compiled server as the development server: the
 * executable with `binArgs`, or the command that `wrapCmdFn` returns for them.
 * It fails when `wrapCmdFn` returns anything other than an executable followed
 * by arguments.
 */
function getDevServerCmd(input: {
	readonly server: Pick<ResolvedDevServer, "binArgs" | "wrapCmdFn">;
	readonly executable: string;
}) {
	const { binArgs, wrapCmdFn } = input.server;
	if (wrapCmdFn === undefined) {
		return Effect.succeed([input.executable, ...binArgs] as const);
	}
	const cmd: unknown = wrapCmdFn({
		executable: input.executable,
		args: binArgs,
	});
	return testCmd(cmd)
		? Effect.succeed(cmd)
		: Effect.fail({
				msg: "dev.server.wrapCmdFn must return an executable followed by its arguments, as an array of strings",
			});
}

export function superviseApplicationProcess(input: {
	readonly server: Pick<ResolvedDevServer, "binArgs" | "wrapCmdFn">;
	readonly configDir: string;
	readonly applicationPort: DevApplicationPort;
	/** The dist directory, in which the server reads the build directory of `vorma dev`. */
	readonly distDir: string;
}) {
	return Effect.gen(function* () {
		const { applicationPort } = input;
		// The processes' fibers belong to a scope of their own. The session
		// scope closes its finalizers last-in first-out, so the stop that is
		// registered after this scope runs before the scope interrupts a
		// process, and the process sees that its end was requested.
		const processScope = yield* Scope.fork(yield* Effect.scope);
		const unexpectedExits = yield* Queue.unbounded<Cause.Cause<unknown>>();
		type Process = {
			readonly fiber: Deferred.Deferred<Fiber.Fiber<never, unknown>>;
			observation: "starting" | "ready" | "exited";
		};
		/**
		 * In `idle`, no process runs and Vorma holds the port, unless holding it
		 * failed. In `stopping`, the process ends, either because the session
		 * stops it or because it exited, and Vorma then holds the port again.
		 */
		type ProcessOwnership =
			| { readonly kind: "idle" }
			| { readonly kind: "running"; readonly process: Process }
			| {
					readonly kind: "stopping";
					readonly process: Process;
					/** Succeeds when the process has ended and Vorma holds the port again. */
					readonly completed: Deferred.Deferred<void>;
			  };
		let ownership: ProcessOwnership = { kind: "idle" };

		/** Holds the port after a process has ended, reporting a failure to hold it as an unexpected exit. */
		const holdPortAfterExit = applicationPort.hold.pipe(
			Effect.catchCause((cause) => Queue.offer(unexpectedExits, cause)),
			Effect.asVoid,
		);

		const stop = Effect.uninterruptible(
			Effect.suspend(() => {
				if (ownership.kind === "idle") {
					return Effect.void;
				}
				if (ownership.kind === "stopping") {
					return Deferred.await(ownership.completed);
				}
				const stopping = {
					kind: "stopping",
					process: ownership.process,
					completed: Deferred.makeUnsafe<void>(),
				} as const;
				ownership = stopping;
				return Deferred.await(stopping.process.fiber).pipe(
					Effect.flatMap(Fiber.interrupt),
					Effect.andThen(holdPortAfterExit),
					Effect.ensuring(
						Effect.sync(() => {
							if (ownership === stopping) {
								ownership = { kind: "idle" };
							}
						}).pipe(
							Effect.andThen(
								Deferred.succeed(stopping.completed, undefined),
							),
						),
					),
				);
			}),
		);
		yield* Effect.addFinalizer(() => stop);

		/**
		 * Starts one process and waits for its readiness; failed startup stops
		 * it. The session starts a process only after it stopped the previous
		 * one.
		 */
		function start(launch: {
			readonly executable: string;
			readonly expectedClientBuildId: string;
		}) {
			return Effect.uninterruptibleMask((restore) => {
				return Effect.gen(function* () {
					if (ownership.kind !== "idle") {
						return yield* Effect.die(
							new Error(
								`The development session started the application while a process was ${ownership.kind}`,
							),
						);
					}
					const cmd = yield* getDevServerCmd({
						server: input.server,
						executable: launch.executable,
					});
					yield* applicationPort.release;
					const ready = yield* Deferred.make<void, unknown>();
					const process: Process = {
						fiber: yield* Deferred.make<
							Fiber.Fiber<never, unknown>
						>(),
						observation: "starting",
					};
					ownership = { kind: "running", process };
					const fiber = yield* Effect.scoped(
						Effect.gen(function* () {
							const server = yield* startDevServerProcess({
								cmd,
								configDir: input.configDir,
								port: applicationPort.port,
								distDir: input.distDir,
								expectedClientBuildId:
									launch.expectedClientBuildId,
								waitingMsgDelay: "10 seconds",
							});
							process.observation = "ready";
							yield* Deferred.succeed(ready, undefined);
							return yield* server.unexpectedExit;
						}),
					).pipe(
						Effect.interruptible,
						Effect.tapCause((cause) => {
							const wasReady = process.observation === "ready";
							process.observation = "exited";
							if (
								ownership.kind === "stopping" &&
								ownership.process === process
							) {
								// The session stops the process and holds the port.
								return Deferred.failCause(ready, cause);
							}
							const exited = {
								kind: "stopping",
								process,
								completed: Deferred.makeUnsafe<void>(),
							} as const;
							ownership = exited;
							return Effect.gen(function* () {
								yield* holdPortAfterExit;
								if (ownership === exited) {
									ownership = { kind: "idle" };
								}
								yield* Deferred.succeed(
									exited.completed,
									undefined,
								);
								yield* Deferred.failCause(ready, cause);
								// An exit before readiness fails `start` instead.
								if (wasReady) {
									yield* Queue.offer(unexpectedExits, cause);
								}
							});
						}),
						Effect.forkIn(processScope),
					);
					yield* Deferred.succeed(process.fiber, fiber);
					yield* restore(Deferred.await(ready)).pipe(
						Effect.onExit((exit) => {
							return Exit.isFailure(exit) ? stop : Effect.void;
						}),
					);
				});
			});
		}

		return {
			start,
			stop,
			testProcessRunning: () => ownership.kind === "running",
			/** Reports each started process's unrequested exit, and each failure to hold the port after a process ended. */
			unexpectedExit: Queue.take(unexpectedExits),
		};
	});
}
