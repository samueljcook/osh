import { normalize } from "node:path";
import { fileURLToPath } from "node:url";
import { Result } from "effect";
import type { Plugin } from "vite";
import type { VormaPluginApi } from "./types.ts";

/** The name by which Vorma's commands find the application's plugin in a resolved Vite configuration. */
export const VORMA_PLUGIN_NAME = "vorma";

/** Runtime peers that must have one browser identity in every Vite invocation. */
export const CLIENT_SHARED_PACKAGES = [
	"@vorma/client",
	"react",
	"react-dom",
	"preact",
	"solid-js",
	"@solidjs/web",
];

/** The path of the configuration file that a configuration's `core.importMetaUrl` identifies. */
export function readConfigFilePath(
	importMetaUrl: string,
): Result.Result<string, string> {
	return Result.try({
		try: () => fileURLToPath(importMetaUrl),
		catch: () => {
			return `core.importMetaUrl must be the configuration file's import.meta.url, got ${JSON.stringify(importMetaUrl)}`;
		},
	});
}

export function findVormaPlugin(
	plugins: ReadonlyArray<Plugin>,
): Plugin<VormaPluginApi> | undefined {
	return plugins.find((plugin) => plugin.name === VORMA_PLUGIN_NAME);
}

/** Checks that a resolved Vite configuration's plugin carries the Vorma configuration file that a command loaded. */
export function checkVormaPlugin(input: {
	readonly plugins: ReadonlyArray<Plugin>;
	readonly configFilePath: string;
}): Result.Result<void, string> {
	const plugin = findVormaPlugin(input.plugins);
	if (plugin?.api === undefined) {
		return Result.fail(
			"Vorma requires createVormaPlugin({ config }) from vorma/vite in the configured Vite plugins list.",
		);
	}
	const { importMetaUrl } = plugin.api.config.core;
	const pluginConfigFilePath = readConfigFilePath(importMetaUrl);
	return Result.isSuccess(pluginConfigFilePath) &&
		normalize(pluginConfigFilePath.success) ===
			normalize(input.configFilePath)
		? Result.succeed(undefined)
		: Result.fail(
				`The Vite configuration's createVormaPlugin({ config }) received the configuration of ${importMetaUrl}, but Vorma loaded ${input.configFilePath}.`,
			);
}
