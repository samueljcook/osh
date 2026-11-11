import { dirname, resolve } from "node:path";
import { Result } from "effect";
import { normalizePath, type Plugin } from "vite";
import type { VormaConfig } from "../config.ts";
import {
	CLIENT_SHARED_PACKAGES,
	VORMA_PLUGIN_NAME,
	readConfigFilePath,
} from "./plugin-api.ts";
import type { VormaPluginApi } from "./types.ts";

/**
 * Connects the application's Vite configuration to its Vorma configuration.
 * In every tool that loads the Vite configuration, the plugin sets Vite's root
 * to the directory of `client.viteConfigFile` and gives Vorma's browser peers
 * one identity. It runs no Vorma build and does not decode the configuration;
 * `vorma build` and `vorma dev` decode it once, do the build work, and check
 * the pairing.
 * Applications must declare Vite 8.3 or later and their renderer's Vite plugin.
 * This plugin does not replace renderer compilation or component refresh.
 *
 * `vorma dev` supports the refresh of these renderer plugins, which it
 * identifies by the plugin names that they give Vite: `@vitejs/plugin-react`
 * (`vite:react-refresh`), Prefresh's `@prefresh/vite` (`prefresh-wrapper`),
 * and `@solidjs/vite-plugin` (`solid`). With another refresh plugin, view
 * updates still use Vorma's hot updates, but Vorma neither reloads the
 * document for an update that affects both views and other modules nor holds
 * the renderer's refresh until Vorma's view updates apply.
 *
 * `vorma dev` needs Vite's watcher and sets `server.watch` itself, so an
 * application's `server.watch: null` has no effect under `vorma dev`.
 * `vorma dev` watches the Vite configuration, the files that the Vite
 * configuration imports, and the env files that Vite reads, and `vorma dev`
 * restarts Vite when any of those files changes; Vite's own watcher ignores
 * those files, so Vite plugins do not receive `watchChange` calls for them.
 * After a restart that a plugin starts with `server.restart()`, `vorma dev`
 * prepares the critical CSS and the document parts again. Vite waits for
 * each plugin's `configureServer`, `buildStart`, and `closeServer` hooks
 * during a restart, so a hook that never settles keeps the update that
 * restarts Vite pending until the developer stops `vorma dev` or changes the
 * Vorma configuration again.
 */
export function createVormaPlugin(options: {
	readonly config: VormaConfig;
}): Plugin<VormaPluginApi> {
	return {
		name: VORMA_PLUGIN_NAME,
		api: { config: options.config },
		config(userConfig) {
			const configFilePath = readConfigFilePath(
				options.config.core.importMetaUrl,
			);
			if (Result.isFailure(configFilePath)) {
				return this.error(configFilePath.failure);
			}
			const root = dirname(
				resolve(
					dirname(configFilePath.success),
					options.config.client.viteConfigFile,
				),
			);
			// Vite resolves a relative root against the working directory.
			if (
				userConfig.root !== undefined &&
				normalizePath(resolve(userConfig.root)) !== normalizePath(root)
			) {
				return this.error(
					`The Vite configuration sets root to ${resolve(userConfig.root)}, but Vorma sets Vite's root to ${root}, the directory of client.viteConfigFile. Remove root.`,
				);
			}
			return {
				root,
				resolve: { dedupe: CLIENT_SHARED_PACKAGES },
			};
		},
	};
}
