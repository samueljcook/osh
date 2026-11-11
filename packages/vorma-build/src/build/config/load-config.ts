import { dirname, normalize, relative } from "node:path";
import { Effect, Path, Result, Schema } from "effect";
import { loadConfigFromFile, type Plugin, type PluginOption } from "vite";
import type { VormaConfig } from "../../config.ts";
import { checkVormaPlugin } from "../../vite/plugin-api.ts";
import { findInvalidDevChangeRules } from "../dev/patterns.ts";
import { describeCause } from "../issues.ts";
import { resolveVormaConfig } from "./resolve-config.ts";
import { VormaConfigSchema } from "./schema.ts";
import { findViteOptionsWithoutEffect } from "./vite-options.ts";

export type ConfigLoadIssue = {
	readonly _tag: "ConfigLoadIssue";
	readonly msg: string;
	readonly cause?: unknown;
};

function createConfigLoadIssue(input: {
	readonly msg: string;
	readonly cause?: unknown;
}): ConfigLoadIssue {
	return {
		_tag: "ConfigLoadIssue",
		msg: input.msg,
		...(input.cause === undefined ? {} : { cause: input.cause }),
	};
}

/** Vite's plugin list nests arrays and promises; the lookup needs the plugins themselves. */
async function flattenPlugins(
	options: ReadonlyArray<PluginOption>,
): Promise<ReadonlyArray<Plugin>> {
	const plugins: Array<Plugin> = [];
	for (const option of options) {
		const resolved = await option;
		if (Array.isArray(resolved)) {
			plugins.push(...(await flattenPlugins(resolved)));
		} else if (typeof resolved === "object" && resolved !== null) {
			plugins.push(resolved as Plugin);
		}
	}
	return plugins;
}

/** Loads one configuration module with Vite's loader, which also reports the files that the module imports. */
function loadConfigModule(input: {
	readonly filePath: string;
	readonly description: string;
	readonly cmd: "build" | "serve";
	readonly mode: "production" | "development";
}) {
	return Effect.tryPromise({
		try: () => {
			return loadConfigFromFile(
				{ command: input.cmd, mode: input.mode },
				input.filePath,
			);
		},
		catch: (cause) => {
			return createConfigLoadIssue({
				msg: `Could not load the ${input.description} ${input.filePath}: ${describeCause(cause)}`,
				cause,
			});
		},
	}).pipe(
		Effect.flatMap((loaded) => {
			return loaded === null
				? Effect.fail(
						createConfigLoadIssue({
							msg: `Could not load the ${input.description} ${input.filePath}`,
						}),
					)
				: Effect.succeed(loaded);
		}),
	);
}

/**
 * Decodes an authored Vorma configuration. The decoder rejects a key that the
 * schema does not define.
 */
export function decodeVormaConfig(input: {
	readonly authored: unknown;
	readonly configFilePath: string;
}): Effect.Effect<VormaConfig, ConfigLoadIssue> {
	return Schema.decodeUnknownEffect(VormaConfigSchema, {
		onExcessProperty: "error",
	})(input.authored).pipe(
		Effect.mapError((cause) => {
			return createConfigLoadIssue({
				msg: `Invalid Vorma configuration in ${input.configFilePath}: ${cause.message}`,
				cause,
			});
		}),
	);
}

/**
 * Loads the Vorma configuration file that a command names, decodes it, and
 * resolves it. A process calls this once and passes the result on. Vite's
 * loader retains the configuration's own `import.meta.url`, so
 * `core.importMetaUrl` must identify the loaded file.
 */
export function loadVormaConfig(input: {
	readonly configPath: string;
	readonly cmd: "build" | "serve";
	readonly mode: "production" | "development";
}) {
	return Effect.gen(function* () {
		const path = yield* Path.Path;
		const configFilePath = path.resolve(input.configPath);
		const configModule = yield* loadConfigModule({
			filePath: configFilePath,
			description: "Vorma configuration",
			cmd: input.cmd,
			mode: input.mode,
		});
		const config = yield* decodeVormaConfig({
			authored: configModule.config,
			configFilePath,
		});
		const invalidRules = findInvalidDevChangeRules(
			config.dev?.onChange ?? [],
		);
		if (invalidRules.length > 0) {
			return yield* Effect.fail(
				createConfigLoadIssue({
					msg: invalidRules
						.map((msg) => `${configFilePath}: ${msg}`)
						.join("\n"),
				}),
			);
		}
		const resolved = yield* resolveVormaConfig(config);
		if (normalize(resolved.configFilePath) !== normalize(configFilePath)) {
			return yield* Effect.fail(
				createConfigLoadIssue({
					msg: `${configFilePath}: core.importMetaUrl must be import.meta.url, got ${JSON.stringify(config.core.importMetaUrl)}`,
				}),
			);
		}
		return {
			config,
			resolved,
			configFilePath,
			/** The configuration file and the files that it imports. */
			dependencies: [configFilePath, ...configModule.dependencies],
		};
	});
}

/** Checks Vite's loaded configuration before Vorma applies its own settings. */
export function createViteConfigCheckPlugin(input: {
	readonly configFilePath: string;
	readonly viteConfigFilePath: string;
}): Plugin {
	return {
		name: "vorma:check-config",
		enforce: "pre",
		config: {
			order: "pre",
			async handler(config) {
				const { configFilePath, viteConfigFilePath } = input;
				const plugins = await flattenPlugins(config.plugins ?? []);
				// Messages name the Vite configuration as the configuration names it.
				const viteConfigFileName = relative(
					dirname(configFilePath),
					viteConfigFilePath,
				);
				const pairing = checkVormaPlugin({ plugins, configFilePath });
				if (Result.isFailure(pairing)) {
					return this.error(
						`${viteConfigFileName}: ${pairing.failure}`,
					);
				}
				const optionsWithoutEffect = findViteOptionsWithoutEffect({
					config,
					fileName: viteConfigFileName,
				});
				if (optionsWithoutEffect.length > 0) {
					return this.error(optionsWithoutEffect.join("\n"));
				}
			},
		},
	};
}

/**
 * Loads the configuration for `vorma dev`, which requires `dev`. The
 * configuration file and its dependencies restart the whole development
 * session; the Vite configuration and its dependencies restart Vite alone.
 */
export function loadDevConfig(configPath: string) {
	return Effect.gen(function* () {
		const loaded = yield* loadVormaConfig({
			configPath,
			cmd: "serve",
			mode: "development",
		});
		const { config, resolved } = loaded;
		if (config.dev === undefined || resolved.dev === undefined) {
			return yield* Effect.fail(
				createConfigLoadIssue({
					msg: `${loaded.configFilePath}: vorma dev requires the dev.server settings`,
				}),
			);
		}

		return {
			...loaded,
			config: { ...config, dev: config.dev },
			resolved: { ...resolved, dev: resolved.dev },
		};
	});
}
