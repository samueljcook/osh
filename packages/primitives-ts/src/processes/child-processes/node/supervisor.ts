import { spawn, type ChildProcess } from "node:child_process";
import { realpathSync } from "node:fs";
import { fileURLToPath, pathToFileURL } from "node:url";
import { callWithResult } from "../../../results/index.ts";

export const SUPERVISOR_ARG = "--primitives-child-process-supervisor";

export type SupervisorMsg =
	| { readonly kind: "started"; readonly pid: number }
	| {
			readonly kind: "exited";
			readonly code: number | null;
			readonly signal: NodeJS.Signals | null;
	  }
	| { readonly kind: "failed"; readonly msg: string };

/**
 * Resolves this module's installed file through the package specifier. The
 * package must be installed as `primitives-ts`, and the runtime must provide
 * synchronous `import.meta.resolve`.
 */
export function getSupervisorModuleUrl(): URL {
	return pathToFileURL(
		realpathSync(
			fileURLToPath(
				import.meta
					.resolve("primitives-ts/processes/child-processes/node"),
			),
		),
	);
}

/** The supervisor remains the group leader until its final group-wide SIGKILL. */
function runSupervisor(): void {
	type ShutdownState =
		| { readonly kind: "open" }
		| { readonly kind: "requested"; readonly timer: NodeJS.Timeout };
	type SupervisorState =
		| { readonly kind: "awaiting-start" }
		| {
				readonly kind: "executing";
				readonly command: ChildProcess;
				readonly graceMillis: number;
				shutdown: ShutdownState;
		  }
		| { readonly kind: "finishing" };
	let state: SupervisorState = { kind: "awaiting-start" };

	function forceTermination(): void {
		// The live group leader prevents reuse of this process-group ID.
		process.kill(-process.pid, "SIGKILL");
	}
	function stop(): void {
		if (state.kind === "awaiting-start") {
			process.exit(0);
		}
		if (state.kind === "finishing" || state.shutdown.kind === "requested") {
			return;
		}
		state.shutdown = {
			kind: "requested",
			timer: setTimeout(forceTermination, state.graceMillis),
		};
		// The supervisor handles its own SIGTERM while the command shuts down.
		process.kill(-process.pid, "SIGTERM");
	}
	function finish(input: {
		readonly msg: SupervisorMsg;
		readonly graceMillis: number;
	}): void {
		if (state.kind === "finishing") {
			return;
		}
		const previous = state;
		state = { kind: "finishing" };
		// A requested shutdown already has a running force-termination timer.
		if (
			previous.kind !== "executing" ||
			previous.shutdown.kind !== "requested"
		) {
			setTimeout(forceTermination, input.graceMillis);
		}
		if (!process.connected) {
			forceTermination();
			return;
		}
		const sent = callWithResult(() =>
			process.send!(input.msg, () => forceTermination()),
		);
		if (sent.kind === "error") {
			forceTermination();
		}
	}
	process.on("SIGTERM", stop);
	process.on("SIGINT", stop);
	process.on("SIGHUP", stop);
	process.on("disconnect", stop);
	process.on(
		"message",
		(
			input:
				| { readonly kind: "stop" }
				| {
						readonly kind: "start";
						readonly cmd: readonly [string, ...Array<string>];
						readonly env: Readonly<
							Record<string, string | undefined>
						>;
						readonly shutdownGraceMillis: number;
				  },
		) => {
			if (input.kind === "stop") {
				stop();
				return;
			}
			if (state.kind !== "awaiting-start" || !process.connected) {
				return;
			}
			const launched = callWithResult(() => {
				return spawn(input.cmd[0], input.cmd.slice(1), {
					env: input.env,
					stdio: ["inherit", "inherit", "inherit"],
					detached: false,
				});
			});
			if (launched.kind === "error") {
				finish({
					msg: { kind: "failed", msg: String(launched.error) },
					graceMillis: input.shutdownGraceMillis,
				});
				return;
			}
			const child = launched.value;
			const executing: Extract<SupervisorState, { kind: "executing" }> = {
				kind: "executing",
				command: child,
				graceMillis: input.shutdownGraceMillis,
				shutdown: { kind: "open" },
			};
			state = executing;
			child.once("error", (error) => {
				finish({
					msg: { kind: "failed", msg: error.message },
					graceMillis: executing.graceMillis,
				});
			});
			child.once("exit", (code, signal) => {
				finish({
					msg: { kind: "exited", code, signal },
					graceMillis: executing.graceMillis,
				});
			});
			child.once("spawn", () => {
				if (state !== executing) {
					return;
				}
				if (!process.connected) {
					stop();
					return;
				}
				const sent = callWithResult(() => {
					return process.send!(
						{ kind: "started", pid: child.pid },
						(error) => {
							if (error) {
								stop();
							}
						},
					);
				});
				if (sent.kind === "error") {
					stop();
				}
			});
		},
	);
	if (!process.connected) {
		stop();
	}
}

// This module also serves as the supervisor executable after bundling.
if (
	process.argv[1] === fileURLToPath(import.meta.url) &&
	process.argv[2] === SUPERVISOR_ARG
) {
	runSupervisor();
}
