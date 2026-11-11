import { request } from "node:http";
import { createConnection } from "node:net";
import { HTTP_PROTOCOL } from "@vorma/client/unstable";
import { Console, type Duration, Effect, Schedule } from "effect";
import { startBuildProcess } from "../process.ts";
import { DEV_PROTOCOL, PROCESS_ENV_PROTOCOL } from "./dev-protocol.ts";

/** The application's server process exited before `vorma dev` stopped it. */
export type DevServerExitIssue = {
	readonly _tag: "DevServerExitIssue";
	readonly exitCode: number;
	readonly msg: string;
};

/** What one readiness probe observed; only `Ready` ends the wait. */
type ReadinessObservation =
	| { readonly _tag: "Ready" }
	| { readonly _tag: "NotReady"; readonly description: string };

/**
 * Sends one GET of the readiness path to `localhost`, as the browser does.
 * `autoSelectFamily` tries each address that `localhost` resolves to, IPv6
 * and IPv4, so the application can listen on either.
 */
function probeReadiness(input: {
	readonly port: number;
	readonly expectedClientBuildId: string;
}): Effect.Effect<ReadinessObservation> {
	return Effect.callback<ReadinessObservation>((resume, signal) => {
		const probe = request(
			{
				host: "localhost",
				port: input.port,
				path: DEV_PROTOCOL.readinessPath,
				method: "GET",
				agent: false,
				createConnection: () => {
					return createConnection({
						host: "localhost",
						port: input.port,
						autoSelectFamily: true,
					});
				},
				signal,
			},
			(response) => {
				response.resume();
				const clientBuildId =
					response.headers[
						HTTP_PROTOCOL.responseHeaders.clientBuildId
					];
				resume(
					Effect.succeed(
						response.statusCode === 200 &&
							clientBuildId === input.expectedClientBuildId
							? { _tag: "Ready" }
							: {
									_tag: "NotReady",
									description: `answered ${response.statusCode} with build ID ${String(clientBuildId)}, expected 200 with build ID ${input.expectedClientBuildId}`,
								},
					),
				);
			},
		);
		probe.on("error", (error) => {
			// A connection that tries several addresses fails with each address's error.
			const msgs =
				error instanceof AggregateError
					? error.errors.map((cause: unknown) => String(cause))
					: [String(error)];
			resume(
				Effect.succeed({
					_tag: "NotReady",
					description: `failed: ${msgs.join("; ")}`,
				}),
			);
		});
		probe.end();
	});
}

/**
 * Runs the configured server command; its Effect scope owns process
 * termination. The process is ready when Vorma's handler answers the
 * readiness path at `http://localhost:<port>` with 200 and the build ID that
 * `vorma dev` published for it. Vorma waits until the process is ready or
 * exits; after `waitingMsgDelay`, it prints the URL that it is waiting on
 * and keeps waiting.
 */
export function startDevServerProcess(input: {
	readonly cmd: readonly [string, ...Array<string>];
	readonly configDir: string;
	readonly port: number;
	/** The dist directory, in which the server reads the build directory of `vorma dev`. */
	readonly distDir: string;
	readonly expectedClientBuildId: string;
	readonly waitingMsgDelay: Duration.Input;
}) {
	return Effect.gen(function* () {
		const childProcess = yield* startBuildProcess({
			cmd: input.cmd,
			configDir: input.configDir,
			environment: {
				[DEV_PROTOCOL.serverPortEnvVar]: String(input.port),
				[DEV_PROTOCOL.devProcessEnvVar]: "1",
				[PROCESS_ENV_PROTOCOL.distDirEnvVar]: input.distDir,
			},
			stdout: "inherit",
			stderr: "inherit",
		});
		const unexpectedExit = childProcess.exitCode.pipe(
			Effect.flatMap((exitCode) => {
				return Effect.fail({
					_tag: "DevServerExitIssue",
					exitCode,
					msg: `The application server ${input.cmd[0]} exited with code ${exitCode}. Correct the problem that it reported above; Vorma builds and starts the server again after the next change to a file that it watches.`,
				} satisfies DevServerExitIssue);
			}),
		);
		const url = `http://localhost:${input.port}`;
		let lastObservation = "no probe completed";
		const observe = probeReadiness({
			port: input.port,
			expectedClientBuildId: input.expectedClientBuildId,
		}).pipe(
			Effect.timeoutOrElse({
				duration: "1 second",
				orElse: () => {
					return Effect.succeed<ReadinessObservation>({
						_tag: "NotReady",
						description: "did not answer within 1 second",
					});
				},
			}),
			Effect.flatMap((observation) => {
				if (observation._tag === "Ready") {
					return Effect.void;
				}
				lastObservation = observation.description;
				return Effect.fail(observation);
			}),
		);
		yield* observe.pipe(
			Effect.retry(
				Schedule.forever.pipe(
					Schedule.modifyDelay(({ output }) => {
						return Effect.succeed(Math.min((output + 1) * 20, 200));
					}),
				),
			),
			Effect.raceFirst(unexpectedExit),
			Effect.raceFirst(
				Effect.sleep(input.waitingMsgDelay).pipe(
					Effect.andThen(
						Effect.suspend(() => {
							return Console.error(
								`Vorma is still waiting for the application at ${url}. The application must listen on the port that the ${DEV_PROTOCOL.serverPortEnvVar} environment variable names. The last readiness probe ${lastObservation}.`,
							);
						}),
					),
					Effect.andThen(Effect.never),
				),
			),
		);
		return { childProcess, unexpectedExit };
	});
}
