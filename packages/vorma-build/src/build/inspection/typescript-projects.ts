import { ChildProcess } from "node:child_process";
import { Writable } from "node:stream";
import { Deferred, Duration, Effect, Exit } from "effect";
import {
	API,
	DiagnosticCategory,
	type APIOptions,
	type Project,
} from "typescript/unstable/async";
import { normalizePath } from "vite";
import { describeCause } from "../issues.ts";
import { BUILD_PROCESS_SHUTDOWN_GRACE_MILLIS } from "../process.ts";

/*
The build's reader of client source. TypeScript's checker resolves the client
project the way the editor does: it follows imports and aliases and computes
types, and it executes nothing. One session opens the configured client
project, and every build-time fact about views and static public asset lookups
is read from it while the session is open.
*/

export type TsProjectIssue = {
	readonly _tag: "TsProjectIssue";
	readonly msg: string;
	readonly cause?: unknown;
};

/** Reads the client project's root files without loading its program or declaration types. */
export function readClientProjectRoots(input: {
	readonly configDir: string;
	readonly clientTsConfigFilePath: string;
}) {
	return withTsApi({
		configDir: input.configDir,
		useFn: (api) => {
			return runTsOperation(async () => {
				return (await api.parseConfigFile(input.clientTsConfigFilePath))
					.fileNames;
			});
		},
	});
}

/** Opens the configured client project for the duration of `useFn`, then closes the checker session. */
export function withClientProject<A, E, R>(input: {
	readonly configDir: string;
	readonly clientTsConfigFilePath: string;
	/**
	 * Files that the checker reads as empty. Inspection empties the generated
	 * types file, so it reads every build as the first build does and never
	 * depends on a previous build's output.
	 */
	readonly emptiedFiles: ReadonlyArray<string>;
	readonly fileSystem?: APIOptions["fs"];
	readonly useFn: (client: Project) => Effect.Effect<A, E, R>;
}): Effect.Effect<A, E | TsProjectIssue, R> {
	const emptiedFiles = new Set(input.emptiedFiles.map(normalizePath));
	return withTsApi({
		configDir: input.configDir,
		fileSystem: {
			...input.fileSystem,
			readFile(filePath) {
				const content = input.fileSystem?.readFile?.(filePath);
				return emptiedFiles.has(normalizePath(filePath)) ? "" : content;
			},
		},
		useFn: (api) => {
			return Effect.gen(function* () {
				// Closing the API session disposes the snapshot.
				const snapshot = yield* runTsOperation(() => {
					return api.updateSnapshot({
						openProjects: [input.clientTsConfigFilePath],
					});
				});
				const client = snapshot.getProject(
					input.clientTsConfigFilePath,
				);
				if (client === undefined) {
					return yield* Effect.fail({
						_tag: "TsProjectIssue" as const,
						msg: `TypeScript did not load ${input.clientTsConfigFilePath}`,
					});
				}
				const diagnostics = yield* runTsOperation(() => {
					return client.program.getConfigFileParsingDiagnostics();
				});
				const errors = diagnostics.filter((diagnostic) => {
					return diagnostic.category === DiagnosticCategory.Error;
				});
				if (errors.length > 0) {
					return yield* Effect.fail({
						_tag: "TsProjectIssue" as const,
						msg: errors
							.map(
								(error) =>
									`${error.fileName}: TS${error.code}: ${error.text}`,
							)
							.join("\n"),
					});
				}
				return yield* input.useFn(client);
			});
		},
	});
}

/**
 * One request to the compiler. The compiler has no cancellation API, so an
 * interrupted request stays pending, and the session's release ends the
 * compiler process.
 */
export function runTsOperation<Value>(fn: () => Promise<Value>) {
	return Effect.tryPromise({
		try: fn,
		catch: (cause): TsProjectIssue => {
			return {
				_tag: "TsProjectIssue",
				msg: `Vorma could not read the application's client TypeScript project: ${describeCause(cause)}`,
				cause,
			};
		},
	});
}

/*
The API's requests settle only while its compiler process runs: when the
process exits, for example because a terminal's interrupt reached the whole
process group, a pending request and the API's `close` wait forever. The API
does not expose the process, so a session reads it from the API's client, and
it fails to open when the client does not have the shape of the pinned
`typescript` version.

The API also does not handle a failed write to the process. A write that
fails because the process ended, for example a request that an interrupted
operation sends after its session killed the compiler, rejects a promise that
nothing observes, and Node ends the whole program on it. A session therefore
puts a guard between the API and the process's input: the guard reports the
first failed write or exit as the end of the compiler, and it drops every
write after the compiler ended, which leaves the request pending as an ended
compiler leaves every request.
*/

/** The API's client, which holds the compiler process from the session's first request. */
type ApiClient = { process: unknown };

function readApiClient(api: API): ApiClient | undefined {
	const client: unknown = Reflect.get(api, "client");
	return typeof client === "object" && client !== null && "process" in client
		? client
		: undefined;
}

/** Whether a session's compiler runs, or has ended. */
type CompilerLifetime =
	| { readonly kind: "running" }
	| { readonly kind: "ended" };

/**
 * Guards each compiler process that the client starts, from the moment that
 * the client assigns it, which is before the client reads the process's input.
 * `onEnded` receives, once, a message that says why the compiler ended.
 */
function guardCompilerProcesses(input: {
	readonly client: ApiClient;
	readonly onEnded: (msg: string) => void;
}) {
	let compiler: unknown = input.client.process;
	let lifetime: CompilerLifetime = { kind: "running" };
	function end(msg: string) {
		if (lifetime.kind === "running") {
			lifetime = { kind: "ended" };
			input.onEnded(msg);
		}
	}
	Object.defineProperty(input.client, "process", {
		configurable: true,
		enumerable: true,
		get: () => compiler,
		set: (value: unknown) => {
			compiler = value;
			if (value instanceof ChildProcess && value.stdin !== null) {
				const compilerInput = value.stdin;
				// A write error reaches this listener and the write's callback.
				compilerInput.on("error", () => {});
				value.stdin = new Writable({
					write(chunk, encoding, callback) {
						if (lifetime.kind === "ended") {
							callback();
							return;
						}
						compilerInput.write(chunk, encoding, (error) => {
							if (error !== null && error !== undefined) {
								end(
									`writing to it failed with ${error.message}`,
								);
							}
							callback();
						});
					},
				});
				// The pinned `typescript` client creates its message writer from
				// the process's `stdin` in the same synchronous turn in which it
				// assigns the process, so the writer receives the guard. Node
				// destroys the process's `stdin` property when the process
				// exits, and the guard must stay writable after that, so the
				// property returns to the process's own input in the next
				// microtask.
				queueMicrotask(() => {
					value.stdin = compilerInput;
				});
				value.once("exit", (code, signal) => {
					end(`it exited with ${signal ?? `code ${code}`}`);
				});
				// The client's connection closes when the compiler's output
				// closes, which Node can report before the process's exit. A
				// request that starts after that fails at once, so the end of
				// the output ends the session too.
				value.stdout?.once("close", () => {
					end("its output closed");
				});
			}
		},
	});
}

/** Completes when the compiler process has exited. */
function awaitCompilerExit(compiler: ChildProcess): Effect.Effect<void> {
	return Effect.callback<void>((resume) => {
		if (compiler.exitCode !== null || compiler.signalCode !== null) {
			resume(Effect.void);
			return;
		}
		function onExit() {
			resume(Effect.void);
		}
		compiler.once("exit", onExit);
		return Effect.sync(() => {
			compiler.off("exit", onExit);
		});
	});
}

/**
 * Closes a session that ended. After success, no request is pending, and the
 * session closes the API, which ends the compiler's input, and waits for the
 * compiler to exit. After a failure or an interruption, a request can be
 * pending, so the session ends the compiler. A compiler that cannot close its
 * API, or that has not exited within the shutdown grace period of Vorma's
 * build processes, is also ended: the session's results are already read, and
 * the API does not end a compiler after a failed close.
 */
function closeTsSession(input: {
	readonly api: API;
	readonly client: ApiClient;
	readonly exit: Exit.Exit<unknown, unknown>;
}): Effect.Effect<void> {
	const compiler = input.client.process;
	if (!(compiler instanceof ChildProcess)) {
		return Effect.promise(() => input.api.close());
	}
	const endCompiler = Effect.sync(() => {
		compiler.kill("SIGKILL");
	});
	if (Exit.isFailure(input.exit)) {
		return endCompiler;
	}
	return Effect.tryPromise(() => input.api.close()).pipe(
		Effect.andThen(awaitCompilerExit(compiler)),
		Effect.raceFirst(awaitCompilerExit(compiler)),
		Effect.timeoutOrElse({
			duration: Duration.millis(BUILD_PROCESS_SHUTDOWN_GRACE_MILLIS),
			orElse: () => endCompiler,
		}),
		Effect.catch(() => endCompiler),
	);
}

function withTsApi<A, E, R>(input: {
	readonly configDir: string;
	readonly fileSystem?: APIOptions["fs"];
	readonly useFn: (api: API) => Effect.Effect<A, E, R>;
}): Effect.Effect<A, E | TsProjectIssue, R> {
	return Effect.scoped(
		Effect.gen(function* () {
			const compilerEnded = yield* Deferred.make<TsProjectIssue>();
			const session = yield* Effect.acquireRelease(
				Effect.suspend(() => {
					const api = new API({
						cwd: input.configDir,
						...(input.fileSystem === undefined
							? {}
							: { fs: input.fileSystem }),
					});
					const client = readApiClient(api);
					if (client === undefined) {
						return Effect.fail({
							_tag: "TsProjectIssue" as const,
							msg: "Vorma cannot manage this version of the TypeScript API: its API has no client with a compiler process",
						});
					}
					guardCompilerProcesses({
						client,
						onEnded: (msg) => {
							Deferred.doneUnsafe(
								compilerEnded,
								Exit.succeed({
									_tag: "TsProjectIssue",
									msg: `Vorma could not read the application's client TypeScript project: the compiler process ended during the session, because ${msg}`,
								}),
							);
						},
					});
					return Effect.succeed({ api, client });
				}),
				(session, exit) => {
					return closeTsSession({ ...session, exit });
				},
			);
			// A request to an ended compiler never settles, so the end of the
			// compiler fails the session.
			return yield* input
				.useFn(session.api)
				.pipe(
					Effect.raceFirst(
						Deferred.await(compilerEnded).pipe(
							Effect.flatMap((error) => Effect.fail(error)),
						),
					),
				);
		}),
	);
}
