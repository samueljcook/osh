import { Effect, Path } from "effect";
import { testPathContained } from "primitives-ts/fs/path-containment/node";
import { normalizePath } from "vite";
import { withApplicationInspection } from "../inspection/inspect-application.ts";
import type { ClientInspection } from "../inspection/inspect-projects.ts";
import {
	inspectCompiledRustServer,
	type CompiledRustServer,
} from "../inspection/rust-server.ts";
import { runTsOperation } from "../inspection/typescript-projects.ts";
import type { DevLayout } from "./generated-paths.ts";
import { readServerSourceFiles } from "./server-sources.ts";

export type DevSourceInputs = {
	/** The Rust server's inspection, which an inspection after a client-only change reuses. */
	readonly server: CompiledRustServer;
	/** The files that trigger a Rust server rebuild when they change. */
	readonly serverFiles: ReadonlySet<string>;
	readonly clientFiles: ReadonlySet<string>;
	readonly clientRootFiles: ReadonlySet<string>;
	/** Configuration and resolution inputs of the client project that are not program source files. */
	readonly resolutionFiles: ReadonlySet<string>;
	/** Native compiler directory enumeration, including configured inclusion scopes. */
	readonly dirs: ReadonlySet<string>;
};

/** What the client project contributes to development, as soon as the client's inspection exists. */
export type DevClientSources = {
	readonly inspection: ClientInspection;
	/** See `DevSourceInputs.resolutionFiles`. */
	readonly resolutionFiles: ReadonlySet<string>;
};

/**
 * Inspects declarations and records the server's and the client compiler's
 * inputs without replacing filesystem behavior. `server` reuses the previous
 * inputs' server inspection when no server source changed, so the inspection
 * runs neither Cargo nor the server. `onClientSourcesFn` receives the client's
 * part while the server's inspection can still run.
 */
export function inspectDevSources<
	ClientE extends { readonly _tag: string; readonly msg: string },
	ClientR,
>(input: {
	readonly layout: DevLayout;
	readonly server:
		| { readonly _tag: "Inspect" }
		| {
				readonly _tag: "Reuse";
				readonly previous: DevSourceInputs;
		  };
	readonly onClientSourcesFn: (
		sources: DevClientSources,
	) => Effect.Effect<void, ClientE, ClientR>;
}) {
	const { layout } = input;
	return Effect.suspend(() => {
		const files = new Set<string>();
		const dirs = new Set<string>();
		/** The files that the client compiler read and that are not program source files. */
		function readResolutionFiles(clientFiles: ReadonlySet<string>) {
			return new Set([...files].filter((file) => !clientFiles.has(file)));
		}
		return withApplicationInspection({
			layout,
			readServer:
				input.server._tag === "Reuse"
					? Effect.succeed(input.server.previous.server)
					: inspectCompiledRustServer({
							cargoManifestFilePath:
								layout.dev.server.cargoManifestFilePath,
							cargoBin: layout.dev.server.cargoBin,
							cargoArgs: layout.dev.server.cargoArgs,
							binArgs: layout.dev.server.binArgs,
							configDir: layout.configDir,
						}),
			fileSystem: {
				readFile(filePath) {
					files.add(normalizePath(filePath));
					return undefined;
				},
				getAccessibleEntries(dir) {
					dirs.add(normalizePath(dir));
					return undefined;
				},
			},
			onClientInspectedFn({ inspection, client }) {
				return runTsOperation(() => {
					return client.program.getSourceFileNames();
				}).pipe(
					Effect.flatMap((sourceFileNames) => {
						return input.onClientSourcesFn({
							inspection,
							resolutionFiles: readResolutionFiles(
								new Set(sourceFileNames.map(normalizePath)),
							),
						});
					}),
				);
			},
			useFn({ inspection, client, server }) {
				return Effect.gen(function* () {
					const clientFiles = new Set(
						(yield* runTsOperation(() => {
							return client.program.getSourceFileNames();
						})).map(normalizePath),
					);
					const serverFiles =
						input.server._tag === "Reuse"
							? input.server.previous.serverFiles
							: yield* readServerSourceFiles({
									executable: server.executable,
									cargoManifestFilePath:
										layout.dev.server.cargoManifestFilePath,
									cargo: layout.cargo,
								});
					const inputs: DevSourceInputs = {
						server,
						serverFiles,
						clientFiles,
						clientRootFiles: new Set(
							client.rootFiles.map(normalizePath),
						),
						resolutionFiles: readResolutionFiles(clientFiles),
						dirs,
					};
					return { inspection, inputs };
				});
			},
		});
	});
}

/** Watches native compiler inputs while keeping generated writes out of the event stream. */
export function getDevSourceWatchPaths(input: {
	readonly layout: DevLayout;
	readonly sources: DevSourceInputs;
}) {
	return Effect.gen(function* () {
		const path = yield* Path.Path;
		const paths = new Set([
			...input.sources.serverFiles,
			...input.sources.clientFiles,
			...input.sources.clientRootFiles,
			...input.sources.resolutionFiles,
			...input.sources.dirs,
		]);
		return [...paths].filter((filePath) => {
			return !input.layout.generatedPaths.some((generatedPath) => {
				return testPathContained({
					path,
					dir: generatedPath,
					filePath,
				});
			});
		});
	});
}
