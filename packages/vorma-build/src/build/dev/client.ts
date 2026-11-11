import { readFileSync, statSync } from "node:fs";
import { dirname, extname, join, resolve } from "node:path";
import { parse } from "@babel/parser";
import { Effect, Equal, Result } from "effect";
import { encodeBytes } from "primitives-ts/encoding/binary-to-text";
import { fillRandomBytes } from "primitives-ts/random/secure-bytes";
import { RolldownMagicString } from "rolldown";
import {
	createServer,
	normalizePath,
	type Plugin,
	type ResolvedConfig,
	type ViteDevServer,
} from "vite";
import { checkVormaPlugin } from "../../vite/plugin-api.ts";
import { createCssStaticPublicAssets } from "../assets/css-static-public-assets.ts";
import { createStaticPublicAssetPlugin } from "../assets/static-public-asset-plugin.ts";
import { createViteConfigCheckPlugin } from "../config/load-config.ts";
import { createDevViteOptions } from "../config/vite-options.ts";
import { describeCause } from "../issues.ts";
import { toDevModuleUrl } from "./artifacts.ts";
import {
	DEV_BROWSER_MODULE,
	DEV_INVALIDATION_ACK_EVENT,
	DEV_RECONNECT_PATHNAME,
	renderDevBrowserSource,
	type DevUpdateOutcome,
} from "./browser-source.ts";
import type { DevClientInputs } from "./client-inputs.ts";
import {
	INDEX_HTML_TEMPLATE,
	splitIndexHtml,
	toModuleServerHtml,
	type IndexHtmlParts,
} from "./index-html.ts";
import {
	createDevModuleServer,
	type DevModuleServerAddress,
} from "./module-server.ts";
import { testIgnoredDirPath } from "./watcher.ts";

/** Vite failed to start, to restart, to transform the development document, or to close. */
export type DevClientIssue = {
	readonly _tag: "DevClientIssue";
	readonly operation: "create" | "restart" | "read-index-html" | "close";
	readonly msg: string;
	readonly cause: unknown;
};

/** The renderer for the refresh plugin in the resolved Vite configuration. */
type RefreshRuntime = "react" | "preact" | "solid" | undefined;

/**
 * The refresh plugins that Vorma supports, by the plugin names that they give
 * Vite.
 */
const REFRESH_PLUGIN_RUNTIMES: ReadonlyArray<{
	readonly pluginName: string;
	readonly refreshRuntime: Exclude<RefreshRuntime, undefined>;
}> = [
	{ pluginName: "vite:react-refresh", refreshRuntime: "react" },
	{ pluginName: "prefresh-wrapper", refreshRuntime: "preact" },
	{ pluginName: "solid", refreshRuntime: "solid" },
];

/**
 * The name that `@vitejs/plugin-react-swc` gives its plugins. The plugin turns
 * Vite's oxc transform off, so Vorma cannot keep view modules out of React's
 * refresh, and SWC's refresh wrapper replaces Vorma's view updates.
 */
const UNSUPPORTED_REACT_SWC_PLUGIN_NAME = "vite:react-swc";

/**
 * The files that make Vite restart when they change: the configuration file,
 * the files that it imports, and the env files of the configuration's mode in
 * `envDir`. Vite's watcher ignores them, and Vorma restarts Vite for them.
 */
function listViteRestartFiles(config: ResolvedConfig): ReadonlySet<string> {
	const { envDir } = config;
	const envFiles =
		envDir === false
			? []
			: [
					".env",
					".env.local",
					`.env.${config.mode}`,
					`.env.${config.mode}.local`,
				].map((fileName) => join(envDir, fileName));
	return new Set(
		[
			...(config.configFile === undefined ? [] : [config.configFile]),
			...config.configFileDependencies,
			...envFiles,
		].map(normalizePath),
	);
}

/** What a resolved Vite configuration tells Vorma. */
type ViteFacts = {
	readonly restartFiles: ReadonlySet<string>;
	readonly refreshRuntime: RefreshRuntime;
};

function readViteFacts(config: ResolvedConfig): ViteFacts {
	const refreshEnabled = !config.isProduction && config.server.hmr !== false;
	return {
		restartFiles: listViteRestartFiles(config),
		refreshRuntime: refreshEnabled
			? REFRESH_PLUGIN_RUNTIMES.find((entry) => {
					return config.plugins.some(
						(plugin) => plugin.name === entry.pluginName,
					);
				})?.refreshRuntime
			: undefined,
	};
}

/**
 * One pass of a restart: Vite creates a server and, when nothing fails, puts
 * it in place of its server. Vite's `restart` runs another pass when a restart
 * is requested during one.
 */
type RestartPass = {
	/** The servers that `configureServer` received since Vite last had one server in place, in order. */
	readonly createdServers: ReadonlyArray<ViteDevServer>;
};

/** Starts Vite for one inspected application; the surrounding Effect scope closes it. */
export function startDevClient(input: {
	readonly inputs: DevClientInputs;
	readonly address: DevModuleServerAddress;
	/** Records a client source that changed since the last inspection so that the next batch inspects the client project. */
	readonly recordChangeFn: (filePath: string) => void;
	/** Called after a restart that a plugin started has replaced Vite's server. */
	readonly onPluginRestartFn: () => void;
	/** The Vorma configuration and its dependencies; a change restarts the session, not Vite alone. */
	readonly configFiles: ReadonlySet<string>;
}) {
	const { address, recordChangeFn, onPluginRestartFn, configFiles } = input;
	let inputs = input.inputs;
	return Effect.gen(function* () {
		const moduleServer = yield* createDevModuleServer(address);
		/**
		 * The lifecycle of Vite's server. Vorma restarts Vite for every file
		 * that Vite's own watcher would restart it for, so a restart that Vorma
		 * did not start is one that a plugin started. During a restart,
		 * `configureServer` receives the new server and `closeServer` reports
		 * that Vite closed the old server to put the new one in its place. A
		 * pass that ends without `closeServer` failed after Vite created its
		 * server; Vorma closes the servers of such a pass when Vorma's restart
		 * ends, and for a plugin's restart at the next `closeServer` with
		 * `restart` or at Vorma's next restart. The module server hands
		 * WebSocket upgrades only to the server that Vorma activated, so a
		 * server of a failed pass never receives a connection.
		 */
		type ViteServerState =
			| { readonly kind: "creating" }
			| { readonly kind: "active" }
			| {
					readonly kind: "vorma-restart-creating";
					readonly pass: RestartPass;
					/** An earlier pass of the same restart of Vorma's replaced the server. */
					readonly replacedServer: boolean;
			  }
			| { readonly kind: "vorma-restart-replacing" }
			| { readonly kind: "plugin-restarting"; readonly pass: RestartPass }
			| { readonly kind: "closing" }
			| { readonly kind: "closed" };
		let serverState: ViteServerState = { kind: "creating" };
		/**
		 * Every server that `configureServer` received and that Vorma has not
		 * closed. A server that a restart put in place forwards to the server
		 * that `createServer` returned; the server of a failed pass stays
		 * separate until Vorma ends the pass.
		 */
		const observedServers = new Set<ViteDevServer>();
		/**
		 * Ends a restart pass: closes every server that the pass created except
		 * `replacement`, which Vite puts in place.
		 */
		function endRestartPass(endedPass: {
			readonly pass: RestartPass;
			readonly replacement: ViteDevServer | undefined;
		}): void {
			for (const created of endedPass.pass.createdServers) {
				if (created === endedPass.replacement) {
					continue;
				}
				observedServers.delete(created);
				void closeObservedServer(created).then((issue) => {
					if (issue !== undefined) {
						created.config.logger.error(issue.msg);
					}
				});
			}
		}
		/**
		 * What the latest resolved Vite configuration tells Vorma. When the last
		 * pass of Vorma's restart fails, Vorma takes the facts of the server in
		 * place again and keeps the failed configuration's restart files with
		 * them so that a fix of a file that only the failed configuration
		 * imports restarts Vite.
		 */
		let viteFacts:
			| { readonly kind: "unresolved" }
			| ({ readonly kind: "resolved" } & ViteFacts) = {
			kind: "unresolved",
		};
		function restoreActiveViteFacts(activeConfig: ResolvedConfig) {
			const active = readViteFacts(activeConfig);
			viteFacts = {
				kind: "resolved",
				refreshRuntime: active.refreshRuntime,
				restartFiles: new Set([
					...active.restartFiles,
					...(viteFacts.kind === "resolved"
						? viteFacts.restartFiles
						: []),
					...scanViteConfigLocalImports({
						configDir: dirname(inputs.configFilePath),
						viteConfigFilePath: inputs.viteConfigFilePath,
					}),
				]),
			};
		}
		type InspectionResult = {
			readonly shouldReload: boolean;
			readonly failed: boolean;
		};
		type InspectionState =
			| { readonly kind: "idle" }
			| {
					readonly kind: "waiting";
					readonly completion: PromiseWithResolvers<InspectionResult>;
					readonly reload: boolean;
					/** The files for which Vite's loads or watcher found changed text. */
					readonly requestedFiles: ReadonlySet<string>;
			  }
			| { readonly kind: "closed" };
		let sourceRevision = 0;
		let inspection: InspectionState = { kind: "idle" };
		function getInspection() {
			return inspection.kind === "waiting"
				? inspection.completion.promise
				: undefined;
		}
		function requestInspection(
			filePath: string,
		): Promise<InspectionResult> {
			if (inspection.kind === "closed") {
				return Promise.resolve({ shouldReload: true, failed: true });
			}
			sourceRevision += 1;
			const file = normalizePath(filePath);
			inspection =
				inspection.kind === "idle"
					? {
							kind: "waiting",
							completion:
								Promise.withResolvers<InspectionResult>(),
							reload: false,
							requestedFiles: new Set([file]),
						}
					: {
							...inspection,
							requestedFiles: new Set([
								...inspection.requestedFiles,
								file,
							]),
						};
			const pending = inspection.completion.promise;
			// Every request schedules work, including a request after an update reads its sources.
			recordChangeFn(filePath);
			return pending;
		}
		/** Holds changed source loads until the session inspects their static asset lookups. */
		function pauseInspection(filePath: string) {
			if (
				inputs.clientSourceTexts[normalizePath(filePath)] !== undefined
			) {
				void requestInspection(filePath);
			}
		}
		function resumeInspection(
			result: InspectionResult & { readonly revision: number },
		) {
			if (inspection.kind !== "waiting") {
				return;
			}
			inspection = {
				...inspection,
				reload: inspection.reload || result.shouldReload,
			};
			if (result.revision !== sourceRevision) {
				return;
			}
			const pending = inspection;
			inspection = { kind: "idle" };
			pending.completion.resolve({
				shouldReload: pending.reload,
				failed: result.failed,
			});
		}
		function logIssue(issue: DevClientIssue) {
			return Effect.logError(issue.msg);
		}
		function closeObservedServer(server: ViteDevServer) {
			return server.close().then(
				() => undefined,
				(cause: unknown) => {
					return {
						_tag: "DevClientIssue",
						operation: "close",
						msg: `Vite could not close a development server: ${describeCause(cause)}`,
						cause,
					} satisfies DevClientIssue;
				},
			);
		}

		/** Admits the live critical CSS update under a strict `style-src`; the application's own `html.cspNonce` replaces it. */
		const nonceBytes = new Uint8Array(16);
		const nonceResult = fillRandomBytes(nonceBytes);
		if (nonceResult.kind === "error") {
			return yield* Effect.fail({
				_tag: "DevClientIssue",
				operation: "create",
				msg: `Vorma could not create the development style nonce: ${describeCause(nonceResult.error)}`,
				cause: nonceResult.error,
			} satisfies DevClientIssue);
		}
		const sessionCspNonce = encodeBytes(nonceBytes, {
			encoding: "base64",
		});
		const devPlugin: Plugin = {
			name: "vorma:development",
			enforce: "pre",
			config(userConfig) {
				const viteOptions = createDevViteOptions({
					origin: moduleServer.origin,
					port: moduleServer.port,
					server: moduleServer.server,
				});
				const oxc = {
					jsxRefreshInclude: /\.[cm]?[jt]sx?(?:$|\?)/,
					// The entry starts the client, so an update that reaches it
					// reloads the page instead of running it again. Views take
					// updates through the accept handler that `transform` adds.
					jsxRefreshExclude: [
						/\/node_modules\//,
						...[
							inputs.clientEntryFilePath,
							...inputs.clientViewModules,
						].map((file) => {
							return new RegExp(
								`^${normalizePath(file).replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(?:\\?.*)?$`,
							);
						}),
					],
				};
				// Vite merges what this hook returns with the application's
				// configuration, so the hook returns only Vorma's own additions.
				return {
					...viteOptions,
					html: {
						cspNonce: userConfig.html?.cspNonce || sessionCspNonce,
					},
					// An application that sets `oxc: false` has no oxc transform to configure.
					...(userConfig.oxc === false ? {} : { oxc }),
					optimizeDeps: {
						entries: [
							inputs.clientEntryFilePath,
							...inputs.clientViewModules,
						],
					},
					server: {
						...viteOptions.server,
						// The browser must never reuse a stored copy of a module
						// that a hot update replaced: a module that the client
						// imports dynamically by its plain URL, without Vite's
						// timestamp query, must load its current version.
						headers: { "cache-control": "no-store" },
						watch: {
							// Vite concatenates arrays, so a list keeps the application's
							// own `ignored` rule, whether it is a list or a function.
							ignored: [
								(filePath: string) => {
									const file = normalizePath(filePath);
									return (
										testIgnoredDirPath({
											configDir: dirname(
												inputs.configFilePath,
											),
											filePath: file,
										}) ||
										// Rust sources are not browser modules.
										file.endsWith(".rs") ||
										configFiles.has(file) ||
										// Vorma restarts Vite for these files itself.
										(viteFacts.kind === "resolved" &&
											viteFacts.restartFiles.has(file)) ||
										inputs.coordinatedResolutionFiles.has(
											file,
										) ||
										inputs.generatedPaths.some(
											(generatedPath) => {
												return (
													file ===
														normalizePath(
															generatedPath,
														) ||
													file.startsWith(
														`${normalizePath(generatedPath)}/`,
													)
												);
											},
										)
									);
								},
							],
						},
					},
				};
			},
			configureServer(server) {
				if (
					serverState.kind === "closing" ||
					serverState.kind === "closed"
				) {
					// A restart that began before the close created this server.
					void closeObservedServer(server).then((issue) => {
						if (issue !== undefined) {
							server.config.logger.error(issue.msg);
						}
					});
					return;
				}
				observedServers.add(server);
				if (serverState.kind === "active") {
					serverState = {
						kind: "plugin-restarting",
						pass: { createdServers: [server] },
					};
					// The restart closes every browser's connection when it
					// replaces the server, and the update that follows tells
					// those browsers when to reload.
					moduleServer.holdReconnects();
				} else if (serverState.kind === "vorma-restart-replacing") {
					// A plugin restarted Vite during Vorma's restart, so Vite
					// runs another pass before the restart ends.
					serverState = {
						kind: "vorma-restart-creating",
						pass: { createdServers: [server] },
						replacedServer: true,
					};
				} else if (
					serverState.kind === "vorma-restart-creating" ||
					serverState.kind === "plugin-restarting"
				) {
					serverState = {
						...serverState,
						pass: {
							...serverState.pass,
							createdServers: [
								...serverState.pass.createdServers,
								server,
							],
						},
					};
				}
				server.environments.client.hot.on(
					"vite:invalidate",
					({ path }, client) => {
						// Vite handles invalidation synchronously. Its update messages precede
						// this acknowledgement, including when it ignores a duplicate request.
						queueMicrotask(() => {
							client.send({
								type: "custom",
								event: DEV_INVALIDATION_ACK_EVENT,
								data: { path },
							});
						});
					},
				);
				// Vite's CORS and host checks run before this middleware, which
				// receives the imports of the reconnect module also while the
				// module server holds requests.
				server.middlewares.use(
					DEV_RECONNECT_PATHNAME,
					(_request, response) => {
						moduleServer.answerReconnect(response);
					},
				);
				server.watcher.on("change", pauseInspection);
				server.watcher.on("unlink", pauseInspection);
			},
			closeServer({ reason }) {
				// Vite calls this hook with `restart` after it closed the server
				// that a pass replaces; the pass's last server takes its place,
				// and the earlier servers of the pass failed.
				if (reason !== "restart") {
					return;
				}
				if (
					serverState.kind !== "vorma-restart-creating" &&
					serverState.kind !== "plugin-restarting"
				) {
					return;
				}
				const { pass } = serverState;
				endRestartPass({
					pass,
					replacement: pass.createdServers.at(-1),
				});
				// Vite closed the old server, which removed its WebSocket
				// listener, and the replacement registered the latest one.
				moduleServer.activateUpgradeCandidate();
				if (serverState.kind === "vorma-restart-creating") {
					serverState = { kind: "vorma-restart-replacing" };
					return;
				}
				serverState = { kind: "active" };
				onPluginRestartFn();
			},
			configResolved: {
				order: "post",
				handler(config) {
					viteFacts = { kind: "resolved", ...readViteFacts(config) };
					const checked = checkVormaPlugin({
						plugins: config.plugins,
						configFilePath: inputs.configFilePath,
					});
					if (Result.isFailure(checked)) {
						this.error(checked.failure);
					}
					if (
						config.plugins.some((plugin) => {
							return (
								plugin.name ===
								UNSUPPORTED_REACT_SWC_PLUGIN_NAME
							);
						})
					) {
						this.error(
							"Vorma does not support @vitejs/plugin-react-swc because it turns off the oxc transform with which vorma dev keeps view modules out of React's refresh. Use @vitejs/plugin-react, the React plugin that Vorma supports.",
						);
					}
				},
			},
			resolveId(source) {
				return source === DEV_BROWSER_MODULE ? source : null;
			},
			async load(id) {
				if (id === DEV_BROWSER_MODULE) {
					if (viteFacts.kind === "unresolved") {
						return this.error(
							"Vite loaded Vorma's development browser module before it resolved its configuration",
						);
					}
					return renderDevBrowserSource({
						refreshRuntime: viteFacts.refreshRuntime,
						reconnectModuleUrl:
							moduleServer.origin + DEV_RECONNECT_PATHNAME,
					});
				}
				if (
					inputs.clientSourceTexts[id.split("?", 1)[0] ?? id] !==
					undefined
				) {
					await getInspection();
				}
				return null;
			},
			async hotUpdate() {
				if ((await getInspection())?.shouldReload) {
					return [];
				}
			},
			transform(code, id) {
				const viewModule = inputs.clientViewModules.some((file) => {
					return normalizePath(file) === id;
				});
				if (!viewModule && id !== inputs.clientEntryFilePath) {
					return null;
				}
				const source = new RolldownMagicString(code);
				// Vorma replaces view definitions and reloads an updated application entry.
				// Solid's supported directive keeps its refresh transform out of those modules.
				if (
					viteFacts.kind === "resolved" &&
					viteFacts.refreshRuntime === "solid"
				) {
					source.prepend("/* @refresh skip */\n");
				}
				if (!viewModule) {
					return {
						code: source.toString(),
						map: source.generateMap({ hires: true }).toString(),
					};
				}
				source.append(
					`\nimport { registerViewModule as __vorma_registerViewModule, updateViewModule as __vorma_updateViewModule } from ${JSON.stringify(DEV_BROWSER_MODULE)};\n__vorma_registerViewModule(import.meta.url);\nif (import.meta.hot) { import.meta.hot.accept((module) => { if (module) __vorma_updateViewModule(import.meta.url, module); }); }`,
				);
				return {
					code: source.toString(),
					map: source.generateMap({ hires: true }).toString(),
				};
			},
		};
		const publicCss = createCssStaticPublicAssets(inputs);
		function getStaticPublicAssetPluginInput(application: DevClientInputs) {
			return {
				lookups: application.staticPublicAssetLookups,
				sourceTexts: new Map(
					Object.entries(application.clientSourceTexts),
				),
				staticPublicAssets: application.staticPublicAssets,
			};
		}
		const staticPublicAssetPlugin = createStaticPublicAssetPlugin(
			getStaticPublicAssetPluginInput(inputs),
			{
				kind: "development",
				inspectChangedSourceFn: async (filePath) =>
					!(await requestInspection(filePath)).failed,
			},
		);
		const server = yield* Effect.acquireRelease(
			Effect.tryPromise({
				try: () => {
					return createServer({
						configFile: inputs.viteConfigFilePath,
						plugins: [
							createViteConfigCheckPlugin({
								configFilePath: inputs.configFilePath,
								viteConfigFilePath: inputs.viteConfigFilePath,
							}),
							devPlugin,
							...publicCss.plugins,
							staticPublicAssetPlugin,
						],
					});
				},
				catch: (cause): DevClientIssue => {
					return {
						_tag: "DevClientIssue",
						operation: "create",
						msg: `Vite could not start with ${inputs.viteConfigFilePath}: ${describeCause(cause)}. Correct the Vite configuration or the plugin that failed.`,
						cause,
					};
				},
			}),
			(server) => {
				return Effect.gen(function* () {
					// The session's updates have ended, so no restart of Vorma's
					// runs. A plugin's restart can fail without an end, so the
					// close does not wait for it, and it closes every server
					// that a restart created.
					serverState = { kind: "closing" };
					// Vite's optimizer must finish its initial import crawl before close cancels that crawl.
					yield* Effect.tryPromise({
						try: () => {
							return Promise.all(
								Object.values(server.environments).map(
									(environment) => {
										return environment.waitForRequestsIdle();
									},
								),
							);
						},
						catch: (cause): DevClientIssue => {
							return {
								_tag: "DevClientIssue",
								operation: "close",
								msg: `Vite's pending requests failed before Vite closed: ${describeCause(cause)}`,
								cause,
							};
						},
					}).pipe(Effect.catch(logIssue));
					const issues = yield* Effect.promise(() => {
						return Promise.all(
							[...new Set([server, ...observedServers])].map(
								closeObservedServer,
							),
						);
					});
					yield* Effect.forEach(
						issues.filter((issue) => issue !== undefined),
						logIssue,
						{ discard: true },
					);
				}).pipe(
					Effect.ensuring(
						Effect.sync(() => {
							serverState = { kind: "closed" };
						}),
					),
				);
			},
		);
		moduleServer.activateUpgradeCandidate();
		serverState = { kind: "active" };
		yield* Effect.addFinalizer(() => {
			return Effect.sync(() => {
				const pending = inspection;
				inspection = { kind: "closed" };
				if (pending.kind === "waiting") {
					pending.completion.resolve({
						shouldReload: true,
						failed: true,
					});
				}
			});
		});
		moduleServer.setHandler((request, response) => {
			server.middlewares(request, response, () =>
				response.writeHead(404).end(),
			);
		});
		return {
			server,
			/**
			 * Restarts Vite through its public `restart`, which reads the Vite
			 * configuration file again and creates new plugin instances. The
			 * module server holds requests from the start of the restart until
			 * `endUpdate` so that no request reaches a server that Vite closes,
			 * and the browsers with connections that the restart closes wait for the
			 * outcome of the update. Vite runs another pass when a plugin
			 * requests a restart during one. The restart succeeds when a pass
			 * replaced the server: when a later pass failed, Vite runs the
			 * configuration of the pass that succeeded, and Vorma prints that a
			 * later pass failed. It fails when no pass replaced the server.
			 * When the restart ends with a pass that did not replace the server,
			 * or when the session's end interrupts it, Vorma closes the servers
			 * of that pass; a plugin's restart that did not replace the server
			 * ends with it.
			 * Vite waits for each plugin's `configureServer`, `buildStart`, and
			 * `closeServer` hooks, so a hook that never settles keeps the restart
			 * and its update pending until the developer stops `vorma dev` or
			 * changes the Vorma configuration, which ends the session.
			 */
			restart: Effect.suspend(() => {
				const current = serverState;
				if (
					current.kind !== "active" &&
					current.kind !== "plugin-restarting"
				) {
					return Effect.die(
						new Error(
							`Vorma restarted Vite while Vite's server was ${current.kind}`,
						),
					);
				}
				moduleServer.holdRequests();
				serverState = {
					kind: "vorma-restart-creating",
					pass:
						current.kind === "active"
							? { createdServers: [] }
							: current.pass,
					replacedServer: false,
				};
				return Effect.tryPromise({
					try: () => server.restart(),
					catch: (cause): DevClientIssue => {
						return {
							_tag: "DevClientIssue",
							operation: "restart",
							msg: `Vite could not restart: ${describeCause(cause)}. Correct the Vite configuration ${inputs.viteConfigFilePath} or the plugin that failed.`,
							cause,
						};
					},
				}).pipe(
					Effect.flatMap(() => {
						// Vite logs a failure to create the new server and keeps the
						// server in place.
						const ended = serverState;
						if (ended.kind === "vorma-restart-replacing") {
							return Effect.void;
						}
						if (
							ended.kind === "vorma-restart-creating" &&
							ended.replacedServer
						) {
							return Effect.logWarning(
								`Vite restarted with ${inputs.viteConfigFilePath} and runs the configuration of that restart. A further restart that a plugin requested during it failed, and Vite printed the reason above.`,
							);
						}
						return Effect.fail({
							_tag: "DevClientIssue",
							operation: "restart",
							msg: `Vite could not restart with ${inputs.viteConfigFilePath}, and it printed the reason above. Correct the Vite configuration or the plugin that failed; Vorma restarts Vite after the next change to the Vite configuration or to a file that it imports.`,
							cause: undefined,
						} satisfies DevClientIssue);
					}),
					Effect.ensuring(
						Effect.sync(() => {
							if (
								serverState.kind === "vorma-restart-replacing"
							) {
								serverState = { kind: "active" };
							} else if (
								serverState.kind === "vorma-restart-creating"
							) {
								endRestartPass({
									pass: serverState.pass,
									replacement: undefined,
								});
								restoreActiveViteFacts(server.config);
								serverState = { kind: "active" };
							}
						}),
					),
				);
			}),
			/**
			 * Ends an update that completed or failed: the module server passes
			 * the requests that it held to Vite and answers the browsers that
			 * wait for an update's outcome (see `DEV_RECONNECT_PATHNAME`).
			 */
			endUpdate(outcome: DevUpdateOutcome) {
				return Effect.sync(() => moduleServer.endUpdate(outcome));
			},
			/**
			 * Tells the connected browsers to reload the document. When no
			 * browser is connected, Vite keeps the reload for the next browser
			 * that connects so that a page that loaded during the update
			 * reloads. While browsers wait for the update's outcome, only the
			 * connected browsers receive the reload: the waiting browsers reload
			 * when the update completes, and a kept reload would reload each of
			 * them a second time.
			 */
			reloadBrowsers() {
				if (!moduleServer.testReconnectsWaiting()) {
					server.ws.send({ type: "full-reload" });
					return;
				}
				for (const connected of server.ws.clients) {
					connected.send({ type: "full-reload" });
				}
			},
			origin: moduleServer.origin,
			webSocketOrigin: moduleServer.webSocketOrigin,
			/** The value of Vite's `html.cspNonce`, which reaches the development head. */
			get cspNonce() {
				return server.config.html?.cspNonce;
			},
			listen: moduleServer.listen,
			/** The files that make Vite restart; Vorma watches them and restarts Vite for them. */
			getViteRestartFiles: (): ReadonlySet<string> => {
				return viteFacts.kind === "resolved"
					? viteFacts.restartFiles
					: new Set();
			},
			/** The client files with changed text that a pending inspection request names. */
			getInspectionRequestedFiles: (): ReadonlySet<string> => {
				return inspection.kind === "waiting"
					? inspection.requestedFiles
					: new Set();
			},
			resumeInspection,
			getSourceRevision: () => sourceRevision,
			/**
			 * What Vite's plugins add to the document, with every root-relative
			 * script, module preload, and stylesheet URL resolved against the
			 * module server. Vite and its plugins emit root-relative URLs for
			 * backend HTML integration. The head also carries the development
			 * browser module.
			 */
			readIndexHtmlParts: Effect.tryPromise({
				try: () => server.transformIndexHtml("/", INDEX_HTML_TEMPLATE),
				catch: (cause): DevClientIssue => {
					return {
						_tag: "DevClientIssue",
						operation: "read-index-html",
						msg: `Vite could not transform the development document: ${describeCause(cause)}. Correct the plugin with the failing transformIndexHtml hook.`,
						cause,
					};
				},
			}).pipe(
				Effect.flatMap((html) => {
					const parts = splitIndexHtml(html);
					return Result.isFailure(parts)
						? Effect.fail({
								_tag: "DevClientIssue",
								operation: "read-index-html",
								msg: parts.failure,
								cause: undefined,
							} satisfies DevClientIssue)
						: Effect.succeed(parts.success);
				}),
				Effect.map((parts): IndexHtmlParts => {
					const browserModuleUrl = toDevModuleUrl({
						origin: moduleServer.origin,
						root: server.config.root,
						id: DEV_BROWSER_MODULE,
					});
					function toModuleServerParts(html: string) {
						return toModuleServerHtml({
							html,
							moduleServerOrigin: moduleServer.origin,
						});
					}
					return {
						headHtml:
							toModuleServerParts(parts.headHtml) +
							`<script type="module" src="${browserModuleUrl}"></script>`,
						bodyPrependHtml: toModuleServerParts(
							parts.bodyPrependHtml,
						),
						bodyHtml: toModuleServerParts(parts.bodyHtml),
					};
				}),
			),
			/** Replaces inspected sources and asset references within the existing Vite configuration. */
			updateInputs(next: DevClientInputs) {
				const replacedFiles = new Set([
					...Object.keys(inputs.clientSourceTexts),
					...Object.keys(next.clientSourceTexts),
				]);
				const staticPublicAssetsChanged = !Equal.equals(
					inputs.staticPublicAssetUrls,
					next.staticPublicAssetUrls,
				);
				inputs = next;
				staticPublicAssetPlugin.api.updateInput(
					getStaticPublicAssetPluginInput(next),
				);
				publicCss.updateStaticPublicAssets(next);
				for (const environment of Object.values(server.environments)) {
					const graph = environment.moduleGraph;
					for (const filePath of replacedFiles) {
						for (const module of graph.getModulesByFile(filePath) ??
							[]) {
							graph.invalidateModule(module);
						}
					}
					if (staticPublicAssetsChanged) {
						for (const module of graph.idToModuleMap.values()) {
							if (module.type === "css") {
								graph.invalidateModule(module);
							}
						}
					}
				}
			},
		};
	});
}

const LOCAL_CONFIG_EXTENSIONS = [
	".ts",
	".mts",
	".cts",
	".tsx",
	".js",
	".mjs",
	".cjs",
	".jsx",
] as const;

/**
 * Scans relative module imports reachable on disk from the Vite configuration
 * file without throwing so that a newly imported broken helper file is still
 * watched for restart after `server.restart()` fails before `configResolved`.
 */
function scanViteConfigLocalImports(input: {
	readonly configDir: string;
	readonly viteConfigFilePath: string;
}): ReadonlySet<string> {
	const visited = new Set<string>();
	const resolvedImports = new Set<string>();
	const pending = [input.viteConfigFilePath];
	for (
		let currentFile = pending.pop();
		currentFile !== undefined;
		currentFile = pending.pop()
	) {
		const normalized = normalizePath(currentFile);
		if (visited.has(normalized)) {
			continue;
		}
		visited.add(normalized);
		const sourceText = Result.try(() => readFileSync(currentFile, "utf8"));
		if (Result.isFailure(sourceText)) {
			continue;
		}
		const parsed = Result.try(() => {
			return parse(sourceText.success, {
				sourceType: "unambiguous",
				errorRecovery: true,
				plugins: ["typescript", "jsx"],
			});
		});
		if (Result.isFailure(parsed)) {
			continue;
		}
		const currentDir = dirname(currentFile);
		for (const specifier of collectModuleSpecifiers(
			parsed.success.program,
		)) {
			const cleanSpecifier = specifier.replace(/[?#].*$/s, "");
			if (
				!cleanSpecifier.startsWith("./") &&
				!cleanSpecifier.startsWith("../")
			) {
				continue;
			}
			const resolvedPath = resolveLocalModulePath({
				configDir: input.configDir,
				importerDir: currentDir,
				specifier: cleanSpecifier,
			});
			if (resolvedPath !== undefined) {
				resolvedImports.add(normalizePath(resolvedPath));
				pending.push(resolvedPath);
			}
		}
	}
	return resolvedImports;
}

type ParsedAstNode = {
	readonly type?: unknown;
	readonly source?: unknown;
	readonly value?: unknown;
	readonly name?: unknown;
	readonly callee?: unknown;
	readonly arguments?: unknown;
};

function collectModuleSpecifiers(program: unknown): ReadonlyArray<string> {
	const specifiers: Array<string> = [];
	const stack: Array<unknown> = [program];
	for (let node = stack.pop(); node !== undefined; node = stack.pop()) {
		if (typeof node !== "object" || node === null) {
			continue;
		}
		if (Array.isArray(node)) {
			for (const element of node) {
				stack.push(element);
			}
			continue;
		}
		const record = node as ParsedAstNode;
		const { type } = record;
		if (
			(type === "ImportDeclaration" ||
				type === "ExportNamedDeclaration" ||
				type === "ExportAllDeclaration") &&
			typeof record.source === "object" &&
			record.source !== null &&
			(record.source as ParsedAstNode).type === "StringLiteral" &&
			typeof (record.source as ParsedAstNode).value === "string"
		) {
			specifiers.push((record.source as ParsedAstNode).value as string);
		} else if (
			type === "CallExpression" &&
			typeof record.callee === "object" &&
			record.callee !== null &&
			Array.isArray(record.arguments)
		) {
			const callee = record.callee as ParsedAstNode;
			const firstArg = record.arguments[0] as ParsedAstNode | undefined;
			if (
				(callee.type === "Import" ||
					(callee.type === "Identifier" &&
						callee.name === "require")) &&
				typeof firstArg === "object" &&
				firstArg !== null &&
				firstArg.type === "StringLiteral" &&
				typeof firstArg.value === "string"
			) {
				specifiers.push(firstArg.value);
			}
		}
		for (const [key, value] of Object.entries(record)) {
			if (
				key !== "loc" &&
				key !== "comments" &&
				key !== "leadingComments" &&
				key !== "trailingComments" &&
				key !== "innerComments" &&
				typeof value === "object" &&
				value !== null
			) {
				stack.push(value);
			}
		}
	}
	return specifiers;
}

function resolveLocalModulePath(input: {
	readonly configDir: string;
	readonly importerDir: string;
	readonly specifier: string;
}): string | undefined {
	const base = resolve(input.importerDir, input.specifier);
	const extension = extname(base).toLowerCase();
	const stem =
		extension.length > 0 ? base.slice(0, -extension.length) : undefined;
	const remappedCandidates =
		stem === undefined
			? []
			: extension === ".js"
				? [`${stem}.ts`, `${stem}.tsx`]
				: extension === ".mjs"
					? [`${stem}.mts`]
					: extension === ".cjs"
						? [`${stem}.cts`]
						: extension === ".jsx"
							? [`${stem}.tsx`]
							: [];
	const candidates = [
		base,
		...remappedCandidates,
		...LOCAL_CONFIG_EXTENSIONS.map((ext) => `${base}${ext}`),
		...LOCAL_CONFIG_EXTENSIONS.map((ext) => join(base, `index${ext}`)),
	];
	for (const candidate of candidates) {
		if (
			testIgnoredDirPath({
				configDir: input.configDir,
				filePath: candidate,
			})
		) {
			continue;
		}
		const exists = Result.try(() =>
			statSync(candidate, { throwIfNoEntry: false })?.isFile(),
		);
		if (Result.isSuccess(exists) && exists.success === true) {
			return candidate;
		}
	}
	return undefined;
}
