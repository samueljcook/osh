import {
	Cause,
	Console,
	Deferred,
	Effect,
	Equal,
	Exit,
	FileSystem,
	Path,
	Predicate,
	Result,
	Scope,
	Stream,
} from "effect";
import { normalizePath } from "vite";
import { loadDevConfig } from "../config/load-config.ts";
import type {
	ResolvedDev,
	ResolvedVormaConfig,
} from "../config/resolve-config.ts";
import { loadApplicationSourceLayout } from "../config/source-layout.ts";
import {
	describeCause,
	printBuildIssues,
	readBuildIssues,
	type VormaBuildIssues,
} from "../issues.ts";
import { getBuildDirPaths } from "../output/dist-dir.ts";
import { acquireOutputOwnership } from "../output/output-ownership.ts";
import {
	preparePublicOutput,
	writeBuildMetadata,
} from "../publication/publish-artifacts.ts";
import {
	superviseApplicationProcess,
	type DevApplicationPort,
} from "./application-process.ts";
import { assembleDevArtifacts } from "./artifacts.ts";
import { DEV_BROWSER_EVENT, type DevBrowserMsg } from "./browser-source.ts";
import { prepareDevBuild, type DevBuild } from "./build.ts";
import { readCargoMetadata, type CargoMetadata } from "./cargo-metadata.ts";
import type { DevClientInputs } from "./client-inputs.ts";
import { startDevClient, type DevClientIssue } from "./client.ts";
import { getGeneratedPaths, type DevLayout } from "./generated-paths.ts";
import {
	prepareDevGeneration,
	toDevGenerationClientInputs,
} from "./generation.ts";
import {
	createDevModuleServerAddress,
	type DevModuleServerIssue,
} from "./module-server.ts";
import { compileDevChangeRules } from "./patterns.ts";
import {
	APPLICATION_PREFERRED_PORT,
	MODULE_SERVER_PREFERRED_PORT,
	reserveExactPort,
	reserveFirstFreePort,
} from "./ports.ts";
import { getDevSourceWatchPaths } from "./sources.ts";
import { createDevWatcher } from "./watcher.ts";
import {
	classifyDevChanges,
	mergeDevWork,
	NO_DEV_WORK,
	SERVER_WORK,
	STATIC_WORK,
	testDevWorkRequested,
	type DevInputs,
	type DevWork,
} from "./work.ts";

type LoadedConfig = Effect.Success<ReturnType<typeof loadDevConfig>>;

type DevWatcher = Effect.Success<ReturnType<typeof createDevWatcher>>;

type ConfiguredCargoMetadata = Effect.Success<
	ReturnType<typeof readConfiguredCargoMetadata>
>;

/**
 * The application's port and Vorma's reservation of it. Vorma holds the
 * reservation whenever the application does not run, so that a concurrent
 * `vorma dev` process cannot take the port; the application process's
 * supervisor ends it just before the application starts.
 */
type ApplicationPortState =
	| {
			readonly kind: "held";
			readonly port: number;
			/** The configured preference from which the port was chosen. */
			readonly preferredPort: number;
			readonly scope: Scope.Closeable;
	  }
	| {
			readonly kind: "released";
			readonly port: number;
			readonly preferredPort: number;
	  };

/**
 * The module server's port. Vorma reserves it while no module server listens
 * on it, so that a session that fails before it listens leaves the port to
 * the next session. When a module server cannot listen, the port is lost, and
 * the next session reserves another port.
 */
type ModuleServerPortState =
	| {
			readonly kind: "reserved";
			readonly port: number;
			readonly scope: Scope.Closeable;
	  }
	| { readonly kind: "listening"; readonly port: number }
	| { readonly kind: "lost"; readonly port: number };

/** Vorma's lock on the build directory of `vorma dev`. */
type OutputLock = {
	readonly buildDir: string;
	readonly scope: Scope.Closeable;
};

function readPreferredApplicationPort(loaded: LoadedConfig) {
	return loaded.config.dev.server.preferredPort ?? APPLICATION_PREFERRED_PORT;
}

function testDevClientCreateIssue(
	failure: unknown,
): failure is DevClientIssue & { readonly operation: "create" } {
	return (
		Predicate.isTagged(failure, "DevClientIssue") &&
		"operation" in failure &&
		failure.operation === "create" &&
		"msg" in failure &&
		typeof failure.msg === "string"
	);
}

/**
 * Runs until interrupted; configuration replacement releases the complete
 * previous session. Vorma locks the build directory of `vorma dev` for the
 * whole run, and locks the new directory instead when a configuration change
 * moves it. The application's port is chosen from `dev.server.preferredPort`
 * and kept until a configuration change sets another preferred port. Each
 * session reserves its module server's port, starting the search at the
 * previous session's port, or at 5173 for the first session.
 */
export function developApplication(configPath: string) {
	return Effect.gen(function* () {
		const outerScope = yield* Effect.scope;
		let loaded = yield* loadDevConfig(configPath);
		let cargo = yield* readConfiguredCargoMetadata({
			resolved: loaded.resolved,
			lastReported: undefined,
		});
		const watcher = yield* createDevWatcher({
			configFilePath: loaded.configFilePath,
			generatedPaths: cargo.generatedPaths,
		});
		function acquireOutputLock(buildDir: string) {
			return Effect.gen(function* () {
				const scope = yield* Scope.fork(outerScope);
				yield* acquireOutputOwnership({ buildDir, kind: "dev" }).pipe(
					Scope.provide(scope),
					Effect.tapCause(() => Scope.close(scope, Exit.void)),
				);
				return { buildDir, scope } satisfies OutputLock;
			});
		}
		let outputLock = yield* acquireOutputLock(loaded.resolved.dev.buildDir);
		function reserveApplicationPort(preferredPort: number) {
			return Effect.gen(function* () {
				const scope = yield* Scope.fork(outerScope);
				const port = yield* reserveFirstFreePort({
					preferredPort,
					reservationScope: "every-address",
				}).pipe(
					Scope.provide(scope),
					Effect.tapCause(() => Scope.close(scope, Exit.void)),
				);
				return {
					kind: "held",
					port,
					preferredPort,
					scope,
				} satisfies ApplicationPortState;
			});
		}
		let applicationPort: ApplicationPortState =
			yield* reserveApplicationPort(readPreferredApplicationPort(loaded));
		const holdApplicationPort = Effect.suspend(() => {
			const current = applicationPort;
			if (current.kind === "held") {
				return Effect.void;
			}
			return Effect.gen(function* () {
				const scope = yield* Scope.fork(outerScope);
				yield* reserveExactPort({
					port: current.port,
					reservationScope: "every-address",
				}).pipe(
					Scope.provide(scope),
					Effect.tapCause(() => Scope.close(scope, Exit.void)),
				);
				applicationPort = {
					kind: "held",
					port: current.port,
					preferredPort: current.preferredPort,
					scope,
				};
			});
		}).pipe(Effect.uninterruptible);
		const releaseApplicationPort = Effect.suspend(() => {
			const current = applicationPort;
			if (current.kind === "released") {
				return Effect.void;
			}
			applicationPort = {
				kind: "released",
				port: current.port,
				preferredPort: current.preferredPort,
			};
			return Scope.close(current.scope, Exit.void);
		}).pipe(Effect.uninterruptible);
		function reserveModuleServerPort(preferredPort: number) {
			return Effect.gen(function* () {
				const scope = yield* Scope.fork(outerScope);
				const port = yield* reserveFirstFreePort({
					preferredPort,
					reservationScope: "loopback",
				}).pipe(
					Scope.provide(scope),
					Effect.tapCause(() => Scope.close(scope, Exit.void)),
				);
				return {
					kind: "reserved",
					port,
					scope,
				} satisfies ModuleServerPortState;
			});
		}
		let moduleServerPort: ModuleServerPortState =
			yield* reserveModuleServerPort(MODULE_SERVER_PREFERRED_PORT);
		/** Ends the module server port's reservation and listens on the port. */
		function listenModuleServer(
			listen: Effect.Effect<void, DevModuleServerIssue>,
		) {
			return Effect.suspend(() => {
				const current = moduleServerPort;
				if (current.kind !== "reserved") {
					return Effect.die(
						new Error(
							`A development session listened on the module server's port while it was ${current.kind}`,
						),
					);
				}
				return Scope.close(current.scope, Exit.void).pipe(
					Effect.andThen(listen),
					Effect.tap(() => {
						return Effect.sync(() => {
							moduleServerPort = {
								kind: "listening",
								port: current.port,
							};
						});
					}),
					Effect.tapError(() => {
						return Effect.sync(() => {
							moduleServerPort = {
								kind: "lost",
								port: current.port,
							};
						});
					}),
				);
			}).pipe(Effect.uninterruptible);
		}
		let sessionOrder: "initial" | "replacement" = "initial";
		while (true) {
			const preferredPort = readPreferredApplicationPort(loaded);
			if (preferredPort !== applicationPort.preferredPort) {
				const previous = applicationPort;
				if (previous.kind === "held") {
					yield* Scope.close(previous.scope, Exit.void);
				}
				applicationPort = yield* reserveApplicationPort(preferredPort);
			}
			if (loaded.resolved.dev.buildDir !== outputLock.buildDir) {
				yield* Scope.close(outputLock.scope, Exit.void);
				outputLock = yield* acquireOutputLock(
					loaded.resolved.dev.buildDir,
				);
			}
			if (moduleServerPort.kind !== "reserved") {
				// A port that a module server listened on is free again; a lost
				// port belongs to another process, and the search passes it.
				moduleServerPort = yield* reserveModuleServerPort(
					moduleServerPort.port,
				);
			}
			const outcome = yield* Effect.scoped(
				runDevSession({
					loaded,
					cargo,
					watcher,
					applicationPort: {
						port: applicationPort.port,
						hold: holdApplicationPort,
						release: releaseApplicationPort,
					},
					moduleServerPort: moduleServerPort.port,
					listenModuleServerFn: listenModuleServer,
				}),
			).pipe(Effect.result);
			if (Result.isSuccess(outcome)) {
				sessionOrder = "replacement";
				loaded = outcome.success;
				cargo = yield* readConfiguredCargoMetadata({
					resolved: loaded.resolved,
					lastReported: cargo.watchedCargoMetadata,
				});
				continue;
			}
			// When the initial Vite configuration fails to start Vite,
			// developApplication fails so that the CLI exits with status 1.
			if (
				sessionOrder === "initial" &&
				testDevClientCreateIssue(outcome.failure)
			) {
				return yield* Effect.fail(outcome.failure);
			}
			yield* reportSessionFailure(outcome.failure);
			// Another process took the module server's port, so the next
			// session starts at once with another port and a new Vite server.
			if (Predicate.isTagged(outcome.failure, "DevModuleServerIssue")) {
				continue;
			}
			sessionOrder = "replacement";
			// A failed session recovers after an edit.
			while (true) {
				yield* watcher.take;
				const corrected = yield* loadDevConfig(configPath).pipe(
					Effect.result,
				);
				if (Result.isSuccess(corrected)) {
					loaded = corrected.success;
					break;
				}
				yield* reportSessionFailure(corrected.failure);
			}
			cargo = yield* readConfiguredCargoMetadata({
				resolved: loaded.resolved,
				lastReported: cargo.watchedCargoMetadata,
			});
		}
	}).pipe(Effect.scoped);
}

/**
 * Cargo's metadata for the configured manifest, as a result, the metadata
 * that supplies the source paths that the session watches, and the paths that the watcher
 * ignores. Every Cargo build writes the target directory, which is often
 * inside a watched directory, so the watcher learns it before it first scans;
 * while Cargo cannot report it, nothing runs Cargo. When `cargo metadata`
 * fails, the session watches the source paths of `lastReported`, the last
 * metadata that Cargo reported in this `vorma dev` run, so that the fix of a
 * manifest outside the configured directories ends the failed session.
 */
export function readConfiguredCargoMetadata(input: {
	readonly resolved: ResolvedVormaConfig & { readonly dev: ResolvedDev };
	readonly lastReported: CargoMetadata | undefined;
}) {
	const { resolved } = input;
	return Effect.gen(function* () {
		const cargo = yield* readCargoMetadata({
			cargoManifestFilePath: resolved.dev.server.cargoManifestFilePath,
			configDir: resolved.configDir,
		}).pipe(Effect.result);
		return {
			cargo,
			watchedCargoMetadata: Result.isSuccess(cargo)
				? cargo.success
				: input.lastReported,
			generatedPaths: getGeneratedPaths({
				distDir: resolved.distDir,
				clientTypesFilePath: resolved.clientTypesFilePath,
				cargoTargetDir: Result.isSuccess(cargo)
					? cargo.success.targetDir
					: undefined,
			}),
		};
	});
}

/**
 * The paths that hold the server's sources according to Cargo: every local
 * package of the server's build, and the workspace's manifest and lockfile.
 */
function listCargoSourcePaths(input: {
	readonly path: Path.Path;
	readonly cargo: CargoMetadata;
}) {
	return [
		...input.cargo.localPackageDirs,
		input.path.join(input.cargo.workspaceRoot, "Cargo.toml"),
		input.path.join(input.cargo.workspaceRoot, "Cargo.lock"),
	];
}

/** Prints a failure's issue messages, or the whole failure when it reports no issues. */
function reportSessionFailure(failure: unknown): Effect.Effect<void> {
	const issues = readBuildIssues(failure);
	return issues === undefined
		? Effect.logError(failure)
		: printBuildIssues(issues);
}

/** A failure's issues, or the whole cause as one message when the failure reports no issues. */
function describeFailure(cause: Cause.Cause<unknown>): {
	readonly msg: string;
	readonly issues: VormaBuildIssues | undefined;
} {
	const failure = cause.reasons.find(Cause.isFailReason);
	const issues =
		failure === undefined ? undefined : readBuildIssues(failure.error);
	return {
		msg:
			issues === undefined
				? Cause.pretty(cause)
				: issues.issues.map((issue) => issue.msg).join("\n"),
		issues,
	};
}

function runDevSession(input: {
	readonly loaded: LoadedConfig;
	readonly cargo: ConfiguredCargoMetadata;
	readonly watcher: DevWatcher;
	readonly applicationPort: DevApplicationPort;
	readonly moduleServerPort: number;
	/** Ends the module server port's reservation and listens on the port with `listen`. */
	readonly listenModuleServerFn: (
		listen: Effect.Effect<void, DevModuleServerIssue>,
	) => Effect.Effect<void, DevModuleServerIssue>;
}) {
	return Effect.gen(function* () {
		const path = yield* Path.Path;
		const { resolved } = input.loaded;
		const { cargo, watchedCargoMetadata, generatedPaths } = input.cargo;
		const configuredPaths = [
			...input.loaded.dependencies,
			path.dirname(resolved.viteConfigFilePath),
			path.dirname(resolved.clientEntryFilePath),
			path.dirname(resolved.dev.server.cargoManifestFilePath),
			path.dirname(resolved.clientTsConfigFilePath),
			resolved.staticPublicAssetsSourceDir,
			...(resolved.criticalCssEntryFilePath === undefined
				? []
				: [path.dirname(resolved.criticalCssEntryFilePath)]),
			// Cargo reports these paths when the server does not compile, so a
			// session with a failing build sees the change that fixes it.
			...(watchedCargoMetadata === undefined
				? []
				: listCargoSourcePaths({ path, cargo: watchedCargoMetadata })),
		];
		yield* input.watcher.watchPaths({
			paths: configuredPaths,
			generatedPaths,
		});
		const sourceLayout = yield* loadApplicationSourceLayout(resolved);
		if (Result.isFailure(cargo)) {
			return yield* Effect.fail(cargo.failure);
		}
		// The build directory of the layout is the directory of `vorma dev`, so
		// that every step that writes a build writes it there.
		const layout: DevLayout = {
			...sourceLayout,
			...getBuildDirPaths({
				path,
				buildDir: sourceLayout.dev.buildDir,
			}),
			cargo: cargo.success,
			generatedPaths,
		};
		const changeRules = compileDevChangeRules(layout.dev.onChange ?? []);
		yield* preparePublicOutput(layout);

		/**
		 * The updates of the session, one at a time. A running update ends
		 * when server work arrives, and its work joins the queued work; the
		 * state changes in the same synchronous step as the take, the arrival,
		 * and the end, so no work runs twice and no server work misses a
		 * running update. While the running update waits for Vite's restart,
		 * server work waits until the restart ends and then ends the update:
		 * if server work ended the restart, Vite would go on to put a server
		 * that Vorma closed in place of its own. The session begins with the
		 * first start of the application as its running update.
		 */
		type UpdateLoopState =
			| {
					readonly kind: "idle";
					readonly queued: DevWork;
					readonly wake: Deferred.Deferred<void>;
			  }
			| RunningUpdate;
		type RunningUpdate = {
			readonly kind: "running";
			readonly work: DevWork;
			readonly queued: DevWork;
			readonly superseded: Deferred.Deferred<void>;
			readonly phase: "supersedable" | "restarting-vite";
		};
		const firstUpdate: RunningUpdate = {
			kind: "running",
			work: SERVER_WORK,
			queued: NO_DEV_WORK,
			superseded: Deferred.makeUnsafe<void>(),
			phase: "supersedable",
		};
		let updateLoop: UpdateLoopState = firstUpdate;
		/** Queues work; server work ends the running update. */
		function submitWork(work: DevWork): void {
			const current = updateLoop;
			updateLoop = {
				...current,
				queued: mergeDevWork({ left: current.queued, right: work }),
			};
			if (current.kind === "idle") {
				Deferred.doneUnsafe(current.wake, Effect.void);
			} else if (work.serverWork && current.phase === "supersedable") {
				Deferred.doneUnsafe(current.superseded, Effect.void);
			}
		}
		/** Sets the running update's phase; server work that arrived during Vite's restart ends the update after it. */
		function setUpdatePhase(phase: RunningUpdate["phase"]): void {
			const current = updateLoop;
			if (current.kind !== "running") {
				return;
			}
			updateLoop = { ...current, phase };
			if (phase === "supersedable" && current.queued.serverWork) {
				Deferred.doneUnsafe(current.superseded, Effect.void);
			}
		}
		/** Waits for queued work and makes it the running update. */
		const takeWork: Effect.Effect<RunningUpdate> = Effect.suspend(() => {
			const current = updateLoop;
			if (current.kind === "running") {
				return Effect.die(
					new Error("The update loop took work during an update"),
				);
			}
			if (testDevWorkRequested(current.queued)) {
				const running: RunningUpdate = {
					kind: "running",
					work: current.queued,
					queued: NO_DEV_WORK,
					superseded: Deferred.makeUnsafe<void>(),
					phase: "supersedable",
				};
				updateLoop = running;
				return Effect.succeed(running);
			}
			return Deferred.await(current.wake).pipe(Effect.andThen(takeWork));
		});
		/** Runs an update until it ends or server work supersedes it, and then returns to `idle`. */
		function runSupersedableUpdate<R>(
			update: Effect.Effect<void, unknown, R>,
			running: RunningUpdate,
		) {
			return update.pipe(
				Effect.as("completed" as const),
				Effect.raceFirst(
					Deferred.await(running.superseded).pipe(
						Effect.as("superseded" as const),
					),
				),
				Effect.flatMap((outcome) => {
					return Effect.sync(() => {
						const current = updateLoop;
						updateLoop = {
							kind: "idle",
							wake: Deferred.makeUnsafe<void>(),
							// The next update does the work of the one that ended.
							queued:
								outcome === "superseded"
									? mergeDevWork({
											left: running.work,
											right: current.queued,
										})
									: current.queued,
						};
					});
				}),
			);
		}

		const clientInputs = yield* Deferred.make<DevClientInputs>();
		// Vite starts as soon as the client's inspection exists, while Cargo
		// builds the server.
		const [generation, client] = yield* Effect.all(
			[
				prepareDevGeneration({
					layout,
					basis: {
						kind: "first",
						onClientInputsFn: (inputs) => {
							return Deferred.succeed(clientInputs, inputs).pipe(
								Effect.asVoid,
							);
						},
					},
				}),
				Deferred.await(clientInputs).pipe(
					Effect.flatMap((inputs) => {
						return startDevClient({
							inputs,
							address: createDevModuleServerAddress(
								input.moduleServerPort,
							),
							recordChangeFn: input.watcher.recordChange,
							// A plugin's restart can change how Vite processes the
							// critical CSS and what it adds to the document.
							onPluginRestartFn: () => submitWork(STATIC_WORK),
							configFiles: new Set(
								input.loaded.dependencies.map(normalizePath),
							),
						});
					}),
				),
			],
			{ concurrency: 2 },
		);
		const initialBuild = yield* prepareDevBuild({ generation, client });
		/** The last update that completed: what the next update compares with and reuses. */
		let completedUpdate = {
			build: initialBuild,
			artifacts: yield* Effect.fromResult(
				assembleDevArtifacts({
					build: initialBuild,
					client,
					indexHtmlParts: yield* client.readIndexHtmlParts,
				}),
			),
		};

		function sendBrowserMsg(msg: DevBrowserMsg): void {
			if (msg.kind === "full-reload") {
				client.reloadBrowsers();
				return;
			}
			client.server.ws.send({
				type: "custom",
				event: DEV_BROWSER_EVENT,
				data: msg,
			});
		}

		/** Watches a prepared build's inputs and returns them for change classification. */
		function watchBuildInputs(build: DevBuild) {
			return Effect.gen(function* () {
				const buildInputs: DevInputs = {
					sources: build.generation.sources,
					criticalCssFiles: new Set(
						build.stylesheet.inputFilePaths.map(normalizePath),
					),
					configFiles: new Set(
						input.loaded.dependencies.map(normalizePath),
					),
				};
				const sourcePaths = yield* getDevSourceWatchPaths({
					layout,
					sources: buildInputs.sources,
				});
				const changeRuleRoots = changeRules.roots.map((root) =>
					path.resolve(layout.configDir, root),
				);
				yield* input.watcher.watchPaths({
					paths: [
						...sourcePaths,
						...buildInputs.criticalCssFiles,
						...buildInputs.configFiles,
						layout.configFilePath,
						layout.viteConfigFilePath,
						...client.getViteRestartFiles(),
						layout.staticPublicAssetsSourceDir,
						...changeRuleRoots,
					],
					generatedPaths: layout.generatedPaths,
				});
				return buildInputs;
			});
		}

		yield* writeBuildMetadata({
			generation: generation.application,
			artifacts: completedUpdate.artifacts,
		});
		/** The inputs of the latest prepared build, which classify the next batch. */
		let inputs = yield* watchBuildInputs(completedUpdate.build);
		yield* input.listenModuleServerFn(client.listen);
		const application = yield* superviseApplicationProcess({
			server: layout.dev.server,
			configDir: layout.configDir,
			applicationPort: input.applicationPort,
			distDir: layout.distDir,
		});
		/** The session prints the application's URL once, after the application first answers. */
		let announcement: "pending" | "printed" = "pending";
		const announceApplication = Effect.suspend(() => {
			if (announcement === "printed") {
				return Effect.void;
			}
			announcement = "printed";
			return Console.log(
				`Vorma development: http://localhost:${input.applicationPort.port}`,
			);
		});
		function reportFailure(cause: Cause.Cause<unknown>) {
			if (Cause.hasInterruptsOnly(cause)) {
				return Effect.failCause(cause);
			}
			const { msg, issues } = describeFailure(cause);
			sendBrowserMsg({ kind: "error", msg });
			return issues === undefined
				? Effect.logError(msg)
				: printBuildIssues(issues);
		}
		/**
		 * Reports the failure of an update and ends the update with it, which
		 * answers the browsers that wait for an update's outcome. An update that
		 * server work superseded, or that the session's end interrupted, does
		 * not end.
		 */
		function failUpdate(cause: Cause.Cause<unknown>) {
			if (Cause.hasInterruptsOnly(cause)) {
				return Effect.failCause(cause);
			}
			return reportFailure(cause).pipe(
				Effect.andThen(
					client.endUpdate({
						kind: "failed",
						msg: describeFailure(cause).msg,
					}),
				),
			);
		}

		const startFirstApplication = application
			.start({
				executable:
					completedUpdate.build.generation.sources.server.executable,
				expectedClientBuildId:
					completedUpdate.artifacts.metadata.clientBuildId,
			})
			.pipe(
				Effect.andThen(announceApplication),
				Effect.matchCauseEffect({
					onSuccess: () => client.endUpdate({ kind: "completed" }),
					onFailure: failUpdate,
				}),
			);

		/**
		 * Runs one update for the work. Server work stops the server while it
		 * builds and inspects it, and starts it again at the end; client and
		 * static work leave a running server alone. Vite receives the
		 * generation's inputs before it restarts and before it prepares the
		 * critical CSS and the document parts, which need them. The browser
		 * receives `rebuilding` when server work starts and its other messages
		 * after the update completes. A failed update is reported, and no work
		 * stays pending; the published build stays the last completed update's,
		 * while Vite keeps the inputs that the failed update gave it. An update
		 * that completes or fails ends with its outcome, which reaches the
		 * browsers with connections that a restart of Vite closed (see
		 * `DEV_RECONNECT_PATHNAME`); an update that server work supersedes
		 * leaves them waiting for the update that replaces it.
		 */
		function runUpdate(requested: DevWork) {
			return Effect.suspend(() => {
				// A stopped server comes back with the next update, so that no
				// message sends the browser to a server that is not running.
				const work =
					requested.serverWork || application.testProcessRunning()
						? requested
						: { ...requested, serverWork: true };
				const previous = completedUpdate;
				const inspectionRevision =
					work.serverWork || work.clientWork
						? client.getSourceRevision()
						: undefined;
				return Effect.gen(function* () {
					if (work.serverWork) {
						sendBrowserMsg({ kind: "rebuilding" });
					}
					const [generation] = yield* Effect.all(
						[
							prepareDevGeneration({
								layout,
								basis: {
									kind: "update",
									generation: previous.build.generation,
									work,
								},
							}),
							work.serverWork ? application.stop : Effect.void,
						],
						{ concurrency: 2 },
					);
					const nextClientInputs = toDevGenerationClientInputs({
						layout,
						generation,
					});
					// Vite's configuration lists the view modules, so a change of
					// them restarts Vite too.
					const restartsVite =
						work.viteRestart ||
						!Equal.equals(
							toDevGenerationClientInputs({
								layout,
								generation: previous.build.generation,
							}).clientViewModules,
							nextClientInputs.clientViewModules,
						);
					client.updateInputs(nextClientInputs);
					if (restartsVite) {
						// The end of the session can end Vite's restart; server
						// work cannot.
						yield* Effect.acquireUseRelease(
							Effect.sync(() => {
								setUpdatePhase("restarting-vite");
							}),
							() => client.restart,
							() => {
								return Effect.sync(() => {
									setUpdatePhase("supersedable");
								});
							},
						);
					}
					// A restart for a change of the view modules alone changes no
					// plugin, so it reuses the critical CSS as client work does.
					const build = yield* prepareDevBuild({
						generation,
						client,
						previous: { build: previous.build, work },
					});
					const artifacts = yield* Effect.fromResult(
						assembleDevArtifacts({
							build,
							client,
							indexHtmlParts: yield* client.readIndexHtmlParts,
						}),
					);
					yield* writeBuildMetadata({
						generation: build.generation.application,
						artifacts,
					});
					inputs = yield* watchBuildInputs(build);
					if (work.serverWork) {
						yield* application.start({
							executable:
								build.generation.sources.server.executable,
							expectedClientBuildId:
								artifacts.metadata.clientBuildId,
						});
						yield* announceApplication;
					}
					const previousMetadata = previous.artifacts.metadata;
					const metadata = artifacts.metadata;
					const indexHtmlPartsChanged =
						previousMetadata.devHeadHtml !== metadata.devHeadHtml ||
						previousMetadata.devBodyPrependHtml !==
							metadata.devBodyPrependHtml ||
						previousMetadata.devBodyHtml !== metadata.devBodyHtml;
					const shouldReload =
						work.serverWork ||
						previousMetadata.clientBuildId !==
							metadata.clientBuildId ||
						work.staticWork?.reloadsBrowser === true ||
						indexHtmlPartsChanged;
					if (inspectionRevision !== undefined) {
						client.resumeInspection({
							revision: inspectionRevision,
							shouldReload: shouldReload || restartsVite,
							failed: false,
						});
					}
					completedUpdate = { build, artifacts };
					// Vite's restart closed every browser's connection, and each
					// browser reloads the document when it learns that this
					// update completed, so a restart sends no message.
					if (!restartsVite && shouldReload) {
						sendBrowserMsg({ kind: "full-reload" });
					} else if (!restartsVite) {
						if (
							previousMetadata.criticalCss !==
							metadata.criticalCss
						) {
							sendBrowserMsg({
								kind: "css",
								css: metadata.criticalCss,
							});
						}
						sendBrowserMsg({
							kind:
								work.staticWork?.revalidatesClientData === true
									? "revalidate"
									: "ready",
						});
					}
					yield* removeUnpublishedStaticPublicAssets({
						publicDir: layout.publicDir,
						staticPublicAssetFileNames:
							metadata.staticPublicAssetFileNames,
					});
				}).pipe(
					Effect.matchCauseEffect({
						onSuccess: () =>
							client.endUpdate({ kind: "completed" }),
						onFailure: (cause) => {
							if (Cause.hasInterruptsOnly(cause)) {
								return failUpdate(cause);
							}
							// Held loads and hot updates proceed with the last
							// successful inspection, so a changed file that it did
							// not inspect fails to load; the browser shows the error.
							if (inspectionRevision !== undefined) {
								client.resumeInspection({
									revision: inspectionRevision,
									shouldReload: false,
									failed: true,
								});
							}
							return watchBuildInputs(previous.build).pipe(
								Effect.ignore,
								Effect.andThen(failUpdate(cause)),
							);
						},
					}),
				);
			});
		}

		const restartRequest = yield* Deferred.make<LoadedConfig>();
		/** Runs the first start of the application and then the queued work, one update at a time. */
		const runUpdates = runSupersedableUpdate(
			startFirstApplication,
			firstUpdate,
		).pipe(
			Effect.andThen(
				Effect.forever(
					takeWork.pipe(
						Effect.flatMap((running) => {
							return runSupersedableUpdate(
								runUpdate(running.work),
								running,
							);
						}),
					),
				),
			),
		);

		/** Loads the next configuration and ends this session, which the next session replaces. */
		const restartSession = Effect.gen(function* () {
			sendBrowserMsg({ kind: "rebuilding" });
			const nextConfig = yield* loadDevConfig(layout.configFilePath);
			// A configuration that cannot load leaves this session running.
			yield* loadApplicationSourceLayout(nextConfig.resolved);
			yield* Deferred.succeed(restartRequest, nextConfig);
		});

		/**
		 * Sorts a batch into work. A batch that asks for no work does nothing.
		 * Server work ends the update that runs now; client and static work wait
		 * for it to finish. A batch that Vorma cannot sort ends the session as a
		 * failed session, which a later change replaces.
		 */
		function handleBatch(changedPaths: ReadonlySet<string>) {
			return Effect.gen(function* () {
				const classified = yield* classifyDevChanges({
					layout,
					changeRules,
					inputs,
					viteRestartFiles: client.getViteRestartFiles(),
					inspectionRequestedFiles:
						client.getInspectionRequestedFiles(),
					changedPaths,
				});
				if (classified.restartsSession) {
					const restarted = yield* Effect.result(restartSession);
					if (Result.isSuccess(restarted)) {
						return;
					}
					yield* reportFailure(Cause.fail(restarted.failure));
				}
				if (testDevWorkRequested(classified.work)) {
					submitWork(classified.work);
				}
			}).pipe(
				Effect.tapCause((cause) => {
					return Effect.sync(() => {
						if (!Cause.hasInterruptsOnly(cause)) {
							sendBrowserMsg({
								kind: "error",
								msg: describeFailure(cause).msg,
							});
						}
					});
				}),
			);
		}

		const watchBatches = Stream.fromEffectRepeat(input.watcher.take).pipe(
			Stream.runForEach(handleBatch),
		);
		return yield* Deferred.await(restartRequest).pipe(
			Effect.raceFirst(
				Effect.all([watchBatches, runUpdates], {
					concurrency: "unbounded",
					discard: true,
				}).pipe(Effect.andThen(Effect.never)),
			),
			Effect.raceFirst(
				application.unexpectedExit.pipe(
					Effect.flatMap(reportFailure),
					Effect.forever,
				),
			),
		);
	});
}

/**
 * Removes the static public assets that the published metadata does not list.
 * A file that Vorma cannot remove is reported, and the update stays complete.
 */
function removeUnpublishedStaticPublicAssets(input: {
	readonly publicDir: string;
	readonly staticPublicAssetFileNames: ReadonlyArray<string>;
}) {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const path = yield* Path.Path;
		const retainedFiles = new Set(input.staticPublicAssetFileNames);
		const fileNames = yield* fileSystem.readDirectory(input.publicDir).pipe(
			Effect.catch((cause) => {
				return Console.error(
					`Vorma could not read ${input.publicDir} to remove obsolete static public assets: ${describeCause(cause)}`,
				).pipe(Effect.as([]));
			}),
		);
		yield* Effect.forEach(
			fileNames,
			(fileName) => {
				const filePath = path.join(input.publicDir, fileName);
				return retainedFiles.has(fileName)
					? Effect.void
					: fileSystem.remove(filePath).pipe(
							Effect.catch((cause) => {
								return Console.error(
									`Vorma could not remove the obsolete static public asset ${filePath}: ${describeCause(cause)}`,
								);
							}),
						);
			},
			{ concurrency: 16, discard: true },
		);
	});
}
