import type { Readable } from "node:stream";
import { NodeStream } from "@effect/platform-node-shared";
import { Effect, Stream } from "effect";
import { spawnSupervisedChildProcess } from "primitives-ts/processes/child-processes/node";

/** The time that a build or development command has to end after Vorma requests its shutdown, before Vorma ends it by force. */
export const BUILD_PROCESS_SHUTDOWN_GRACE_MILLIS = 2_000;

/**
 * The environment variables that select what an application's server binary
 * does when a build runs it: write its declaration contract, or check a build
 * directory and exit. A command receives such a variable only when the build
 * sets it for that command, and never from Vorma's own environment.
 */
export const SERVER_MODE_ENV_VARS = {
	/** Tells the server binary where to write its declaration contract. */
	contractFile: "__VORMA_DECLARATION_CONTRACT_FILE",
	/** Tells the server binary to load a build directory, check it, and exit. */
	checkBuildDir: "__VORMA_CHECK_BUILD_DIR",
} as const;

/** Vorma's environment without the server mode variables. */
function readInheritedEnvironment(): NodeJS.ProcessEnv {
	const environment = { ...process.env };
	for (const name of Object.values(SERVER_MODE_ENV_VARS)) {
		delete environment[name];
	}
	return environment;
}

/** A Vorma build or development command failed to start, run, or transfer output. */
type BuildProcessIssue = {
	readonly _tag: "BuildProcessIssue";
	readonly msg: string;
};

function createBuildProcessIssue(msg: string): BuildProcessIssue {
	return { _tag: "BuildProcessIssue", msg };
}

function readProcessOutput(stream: Readable | null) {
	return stream === null
		? Stream.empty
		: NodeStream.fromReadable({
				evaluate: () => stream,
				onError: (error) => createBuildProcessIssue(String(error)),
			});
}

/**
 * Runs one build or development command with Vorma's environment, without the
 * server mode variables, and `environment`, and with the shutdown grace period
 * `BUILD_PROCESS_SHUTDOWN_GRACE_MILLIS`. The scope requests shutdown;
 * Primitives also stops the process group if the Vorma launcher dies before
 * scope cleanup.
 */
export function startBuildProcess(input: {
	readonly cmd: readonly [string, ...Array<string>];
	readonly configDir: string;
	readonly environment: Readonly<Record<string, string>>;
	readonly stdout: "inherit" | "pipe";
	readonly stderr: "inherit" | "pipe";
}) {
	return Effect.gen(function* () {
		const child = yield* Effect.acquireRelease(
			Effect.promise(() => {
				return spawnSupervisedChildProcess({
					cmd: input.cmd,
					cwd: input.configDir,
					env: {
						...readInheritedEnvironment(),
						...input.environment,
					},
					stdin: "ignore",
					stdout: input.stdout,
					stderr: input.stderr,
					shutdownGraceMillis: BUILD_PROCESS_SHUTDOWN_GRACE_MILLIS,
				});
			}).pipe(
				Effect.flatMap((result) => {
					return result.kind === "ok"
						? Effect.succeed(result.value)
						: Effect.fail(
								createBuildProcessIssue(result.error.msg),
							);
				}),
			),
			(process) => Effect.promise(() => process.terminate()),
		);
		const exitCode = Effect.promise(() => child.exit).pipe(
			Effect.flatMap((result) => {
				if (result.kind === "error") {
					return Effect.fail(
						createBuildProcessIssue(result.error.msg),
					);
				}
				return result.value.code === null
					? Effect.fail(
							createBuildProcessIssue(
								`Process interrupted due to receipt of signal: '${result.value.signal}'`,
							),
						)
					: Effect.succeed(result.value.code);
			}),
		);
		return {
			exitCode,
			stdout: readProcessOutput(child.stdout),
			stderr: readProcessOutput(child.stderr),
		};
	});
}
