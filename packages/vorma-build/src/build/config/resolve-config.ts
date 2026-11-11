import { Effect, Result } from "effect";
import * as Path from "effect/Path";
import { normalizePath } from "vite";
import type { VormaConfig } from "../../config.ts";
import { readConfigFilePath } from "../../vite/plugin-api.ts";
import {
	ARTIFACT_PROTOCOL,
	getBuildDirPaths,
	type BuildDirPaths,
} from "../output/dist-dir.ts";
import { parseUrlPathname } from "./url-pathname.ts";

export type StaticPublicAssetsServeUrlPathPrefix = "/" | `/${string}/`;

type DevConfig = NonNullable<VormaConfig["dev"]>;

/** The server that `vorma dev` compiles and runs, with its paths resolved. */
export type ResolvedDevServer = Omit<
	DevConfig["server"],
	"cargoManifestFile" | "cargoArgs" | "binArgs"
> & {
	/** The server's Cargo manifest. */
	readonly cargoManifestFilePath: string;
	/** The Cargo binary target that serves the Vorma app. */
	readonly cargoBin: string;
	readonly cargoArgs: ReadonlyArray<string>;
	readonly binArgs: ReadonlyArray<string>;
};

export type ResolvedDev = Omit<DevConfig, "server"> & {
	readonly server: ResolvedDevServer;
	/** The build directory of `vorma dev`, in the dist directory, which the development server reads. */
	readonly buildDir: string;
};

export type ResolvedVormaConfig = {
	/** The configuration file that `core.importMetaUrl` identifies. */
	readonly configFilePath: string;
	/** The directory of the configuration file; every configured path is relative to it. */
	readonly configDir: string;
	/** The command that `vorma build` runs to read the application's declarations. */
	readonly serverStartCmd: VormaConfig["build"]["serverStartCmd"];
	readonly clientTsConfigFilePath: string;
	readonly clientEntryFilePath: string;
	/**
	 * The configured view file pattern, with forward slashes. It is relative to
	 * `configDir` unless it is absolute, also when it starts with `**`, and
	 * matching it never reads a glob character of the directory path as part
	 * of the pattern. A pattern that matches no file of the client project is
	 * an inspection issue.
	 */
	readonly viewFilePattern: string;
	/** Where the build writes the client project's generated types. */
	readonly clientTypesFilePath: string;
	readonly staticPublicAssetsSourceDir: string;
	/** The Vite configuration that Vorma's commands load; its Vorma plugin receives this configuration. */
	readonly viteConfigFilePath: string;
	readonly criticalCssEntryFilePath?: string;
	readonly staticPublicAssetsServeUrlPathPrefix: StaticPublicAssetsServeUrlPathPrefix;
	/** The configured dist directory, which holds the build directory of `vorma build` and the build directory of `vorma dev`. */
	readonly distDir: string;
	readonly dev: ResolvedDev | undefined;
	// The paths of the build directory of `vorma build`.
} & BuildDirPaths;

export type ConfigResolutionIssue = {
	readonly _tag: "ConfigResolutionIssue";
	readonly field:
		| "core.importMetaUrl"
		| "core.staticPublicAssets.serveUrlPathPrefix";
	readonly msg: string;
	readonly cause?: unknown;
};

/**
 * Resolves every configured path once against the configuration file's
 * directory. The configuration is decoded before this call.
 */
export function resolveVormaConfig(
	config: VormaConfig & { readonly dev: DevConfig },
): Effect.Effect<
	ResolvedVormaConfig & { readonly dev: ResolvedDev },
	ConfigResolutionIssue,
	Path.Path
>;
export function resolveVormaConfig(
	config: VormaConfig,
): Effect.Effect<ResolvedVormaConfig, ConfigResolutionIssue, Path.Path>;
export function resolveVormaConfig(
	config: VormaConfig,
): Effect.Effect<ResolvedVormaConfig, ConfigResolutionIssue, Path.Path> {
	return Effect.gen(function* () {
		const path = yield* Path.Path;
		const configFilePath = yield* Effect.fromResult(
			Result.mapError(
				readConfigFilePath(config.core.importMetaUrl),
				(msg): ConfigResolutionIssue => {
					return {
						_tag: "ConfigResolutionIssue",
						field: "core.importMetaUrl",
						msg,
					};
				},
			),
		);
		const configDir = path.dirname(configFilePath);
		function resolvePath(configuredPath: string): string {
			return path.resolve(configDir, configuredPath);
		}
		function resolveDevServer(
			server: DevConfig["server"],
		): ResolvedDevServer {
			const { cargoManifestFile, cargoArgs, binArgs, ...rest } = server;
			return {
				...rest,
				cargoManifestFilePath: resolvePath(cargoManifestFile),
				cargoArgs: cargoArgs ?? [],
				binArgs: binArgs ?? [],
			};
		}
		const distDir = resolvePath(config.core.distDir);
		return {
			configFilePath,
			configDir,
			serverStartCmd: config.build.serverStartCmd,
			clientTsConfigFilePath: resolvePath(config.client.tsConfigFile),
			clientEntryFilePath: resolvePath(config.client.entryFile),
			viewFilePattern: normalizePath(config.client.viewFilePattern),
			clientTypesFilePath: resolvePath(config.client.tsGenOutFile),
			staticPublicAssetsSourceDir: resolvePath(
				config.core.staticPublicAssets.srcDir,
			),
			viteConfigFilePath: resolvePath(config.client.viteConfigFile),
			...(config.client.criticalCssFile === undefined
				? {}
				: {
						criticalCssEntryFilePath: resolvePath(
							config.client.criticalCssFile,
						),
					}),
			staticPublicAssetsServeUrlPathPrefix: yield* Effect.fromResult(
				parseStaticPublicAssetsServeUrlPathPrefix(
					config.core.staticPublicAssets.serveUrlPathPrefix,
				),
			),
			dev:
				config.dev === undefined
					? undefined
					: {
							...config.dev,
							server: resolveDevServer(config.dev.server),
							buildDir: path.join(
								distDir,
								ARTIFACT_PROTOCOL.devBuildDirName,
							),
						},
			distDir,
			...getBuildDirPaths({
				path,
				buildDir: path.join(
					distDir,
					ARTIFACT_PROTOCOL.prodBuildDirName,
				),
			}),
		};
	});
}

function parseStaticPublicAssetsServeUrlPathPrefix(
	value: string,
): Result.Result<StaticPublicAssetsServeUrlPathPrefix, ConfigResolutionIssue> {
	const parsed = parseUrlPathname(value);
	if (Result.isFailure(parsed)) {
		return Result.fail(
			createStaticPublicAssetsServeUrlPathPrefixError(parsed.failure),
		);
	}
	if (!testStaticPublicAssetsServeUrlPathPrefix(parsed.success)) {
		return Result.fail(createStaticPublicAssetsServeUrlPathPrefixError());
	}
	return Result.succeed(parsed.success);
}

/**
 * The root, or an absolute path with a trailing slash, spelled as `encodeURI`
 * spells it. Vite writes each URL under the prefix as `encodeURI` of the
 * decoded prefix and the file name, and the build's own URLs must be equal to
 * Vite's.
 */
function testStaticPublicAssetsServeUrlPathPrefix(
	value: string,
): value is StaticPublicAssetsServeUrlPathPrefix {
	return (
		(value === "/" || (value.startsWith("/") && value.endsWith("/"))) &&
		Result.getOrElse(
			Result.try(() => encodeURI(decodeURI(value)) === value),
			() => false,
		)
	);
}

function createStaticPublicAssetsServeUrlPathPrefixError(
	cause?: unknown,
): ConfigResolutionIssue {
	return {
		_tag: "ConfigResolutionIssue",
		field: "core.staticPublicAssets.serveUrlPathPrefix",
		msg: 'core.staticPublicAssets.serveUrlPathPrefix must be a normalized absolute URL path ending in "/" without an authority, query, or fragment, in which every character that encodeURI encodes is percent-encoded',
		...(cause === undefined ? {} : { cause }),
	};
}
