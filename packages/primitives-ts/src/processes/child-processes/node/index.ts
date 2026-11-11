import { fork, type ChildProcess } from "node:child_process";
import type { Readable, Writable } from "node:stream";
import { fileURLToPath } from "node:url";
import { callWithResult, type Result } from "../../../results/index.ts";
import { getSupervisorModuleUrl, SUPERVISOR_ARG } from "./supervisor.ts";
import type { SupervisorMsg } from "./supervisor.ts";

/** The command's exit status, before the supervisor terminates its remaining descendants. */
export type ChildProcessExit = {
	readonly code: number | null;
	readonly signal: NodeJS.Signals | null;
};

export type ChildProcessError = {
	readonly kind:
		| "invalid-options"
		| "unsupported-platform"
		| "spawn"
		| "supervision";
	readonly msg: string;
	/**
	 * The original error value when this process received one. It is absent for
	 * invalid options, an unsupported platform, a failure that the supervisor
	 * reported as a message from its own process, and a supervisor exit without
	 * a report.
	 */
	readonly cause?: unknown;
};

export type SupervisedChildProcess = {
	readonly pid: number;
	readonly stdin: Writable | null;
	readonly stdout: Readable | null;
	readonly stderr: Readable | null;
	/** Resolves when the supervisor exits after signaling any remaining group members. */
	readonly exit: Promise<Result<ChildProcessExit, ChildProcessError>>;
	/** Requests SIGTERM, waits for the configured grace period, then forces termination. */
	readonly terminate: () => Promise<
		Result<ChildProcessExit, ChildProcessError>
	>;
};

/**
 * Starts a command in an owned Unix process group. An independent supervisor
 * terminates the group if this Node process exits, including after SIGKILL.
 * Closing the handle with `terminate` requests SIGTERM. The supervisor forces
 * termination after `shutdownGraceMillis`, or when the command exits, whichever
 * occurs first. Descendants must not detach into another process group.
 *
 * The command receives exactly `env`. Each standard stream explicitly inherits,
 * ignores, or pipes the corresponding stream. Piped output must be consumed.
 * Node runtime flags are not inherited by the supervisor. Windows is unsupported.
 * TypeScript applications using this Node entry must declare `@types/node`.
 * Browser entries neither import Node modules nor require Node types.
 *
 * The supervisor process runs this package's supervisor module as its
 * executable. This function finds that module with a synchronous
 * `import.meta.resolve` of the specifier
 * `primitives-ts/processes/child-processes/node`, so the package must be
 * installed under the name `primitives-ts` where that specifier resolves from
 * this code, including when an application bundles this code. The runtime must
 * provide synchronous `import.meta.resolve` and `Promise.withResolvers`. When
 * resolution fails, the result is a `spawn` error.
 */
export function spawnSupervisedChildProcess(input: {
	readonly cmd: readonly [string, ...Array<string>];
	readonly cwd: string;
	readonly env: Readonly<Record<string, string | undefined>>;
	readonly stdin: "inherit" | "ignore" | "pipe";
	readonly stdout: "inherit" | "ignore" | "pipe";
	readonly stderr: "inherit" | "ignore" | "pipe";
	readonly shutdownGraceMillis: number;
}): Promise<Result<SupervisedChildProcess, ChildProcessError>> {
	if (process.platform === "win32") {
		return Promise.resolve({
			kind: "error",
			error: {
				kind: "unsupported-platform",
				msg: "Supervised process groups require Unix.",
			},
		});
	}
	if (
		!Number.isSafeInteger(input.shutdownGraceMillis) ||
		input.shutdownGraceMillis < 0 ||
		input.shutdownGraceMillis > 2_147_483_647 ||
		input.cmd.length === 0 ||
		input.cmd[0].length === 0
	) {
		return Promise.resolve({
			kind: "error",
			error: {
				kind: "invalid-options",
				msg: "The command must be nonempty and shutdownGraceMillis must be an integer from 0 to 2147483647.",
			},
		});
	}
	const launched = callWithResult(() => {
		return fork(fileURLToPath(getSupervisorModuleUrl()), [SUPERVISOR_ARG], {
			cwd: input.cwd,
			env: { ...process.env, NODE_OPTIONS: undefined },
			execArgv: [],
			detached: true,
			stdio: [input.stdin, input.stdout, input.stderr, "ipc"],
		});
	});
	if (launched.kind === "error") {
		return Promise.resolve({
			kind: "error",
			error: {
				kind: "spawn",
				msg: String(launched.error),
				cause: launched.error,
			},
		});
	}
	return observeSupervisor({
		supervisor: launched.value,
		cmd: input.cmd,
		env: input.env,
		shutdownGraceMillis: input.shutdownGraceMillis,
	});
}

function observeSupervisor(input: {
	readonly supervisor: ChildProcess;
	readonly cmd: readonly [string, ...Array<string>];
	readonly env: Readonly<Record<string, string | undefined>>;
	readonly shutdownGraceMillis: number;
}): Promise<Result<SupervisedChildProcess, ChildProcessError>> {
	const { supervisor } = input;
	const started =
		Promise.withResolvers<
			Result<SupervisedChildProcess, ChildProcessError>
		>();
	const exited =
		Promise.withResolvers<Result<ChildProcessExit, ChildProcessError>>();
	type ExitResult = Result<ChildProcessExit, ChildProcessError>;
	let observation:
		| { readonly kind: "awaiting-status" }
		| { readonly kind: "reported"; readonly result: ExitResult } = {
		kind: "awaiting-status",
	};
	let termination: "not-requested" | "requested" = "not-requested";
	function recordStatus(result: ExitResult): void {
		observation = { kind: "reported", result };
	}
	function stop() {
		if (supervisor.connected && termination === "not-requested") {
			termination = "requested";
			const sent = callWithResult(() => {
				return supervisor.send({ kind: "stop" }, (error) => {
					if (error && supervisor.connected) {
						supervisor.disconnect();
					}
				});
			});
			if (sent.kind === "error" && supervisor.connected) {
				supervisor.disconnect();
			}
		}
	}
	supervisor.on("error", (error) => {
		recordStatus({
			kind: "error",
			error: { kind: "supervision", msg: error.message, cause: error },
		});
		stop();
	});
	supervisor.on("message", (msg: SupervisorMsg) => {
		if (msg.kind === "started") {
			started.resolve({
				kind: "ok",
				value: {
					pid: msg.pid,
					stdin: supervisor.stdin,
					stdout: supervisor.stdout,
					stderr: supervisor.stderr,
					exit: exited.promise,
					terminate: () => {
						stop();
						return exited.promise;
					},
				},
			});
		} else if (msg.kind === "exited") {
			recordStatus({
				kind: "ok",
				value: { code: msg.code, signal: msg.signal },
			});
		} else {
			recordStatus({
				kind: "error",
				error: { kind: "spawn", msg: msg.msg },
			});
		}
	});
	supervisor.once("close", (_code, signal) => {
		const result: ExitResult =
			observation.kind === "reported"
				? observation.result
				: termination === "requested" && signal === "SIGKILL"
					? { kind: "ok" as const, value: { code: null, signal } }
					: {
							kind: "error" as const,
							error: {
								kind: "supervision" as const,
								msg: "The process supervisor exited without reporting the command's status.",
							},
						};
		exited.resolve(result);
		started.resolve(
			result.kind === "error"
				? result
				: {
						kind: "error",
						error: {
							kind: "supervision",
							msg: "The process supervisor exited before starting the command.",
						},
					},
		);
	});
	const sent = callWithResult(() => {
		return supervisor.send(
			{
				kind: "start",
				cmd: input.cmd,
				env: input.env,
				shutdownGraceMillis: input.shutdownGraceMillis,
			},
			(error) => {
				if (error) {
					recordStatus({
						kind: "error",
						error: {
							kind: "supervision",
							msg: error.message,
							cause: error,
						},
					});
					stop();
				}
			},
		);
	});
	if (sent.kind === "error") {
		recordStatus({
			kind: "error",
			error: {
				kind: "supervision",
				msg: String(sent.error),
				cause: sent.error,
			},
		});
		stop();
	}
	return started.promise;
}
