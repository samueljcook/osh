import { Effect, Result } from "effect";
import type { EnvironmentModuleNode, ViteDevServer } from "vite";
import { minifyCriticalCss } from "../bundling/critical-css.ts";
import { describeCause } from "../issues.ts";
import { toDevModulePath } from "./artifacts.ts";

/** Vite could not prepare the critical CSS file. */
export type DevStylesheetIssue = {
	readonly _tag: "DevStylesheetIssue";
	readonly filePath: string;
	readonly msg: string;
	readonly cause: unknown;
};

function createDevStylesheetIssue(input: {
	readonly filePath: string;
	readonly problem: string;
	readonly cause: unknown;
}): DevStylesheetIssue {
	return {
		_tag: "DevStylesheetIssue",
		filePath: input.filePath,
		msg: `Vorma could not prepare the critical CSS ${input.filePath}: ${input.problem}. Correct the stylesheet or the files that it imports.`,
		cause: input.cause,
	};
}

/** Reads CSS and its dependency files from Vite's native direct-CSS pipeline. */
export function readDevStylesheet(input: {
	readonly server: ViteDevServer;
	readonly filePath: string;
}) {
	return Effect.gen(function* () {
		const environment = input.server.environments.client;
		if (environment === undefined) {
			return yield* Effect.fail(
				createDevStylesheetIssue({
					filePath: input.filePath,
					problem: "Vite has no client environment",
					cause: undefined,
				}),
			);
		}
		const url = `${toDevModulePath({ root: input.server.config.root, id: input.filePath })}?direct`;
		const transformed = yield* Effect.tryPromise({
			try: async () => {
				// Application and Vite watcher callbacks can arrive in either order.
				const previous =
					await environment.moduleGraph.getModuleByUrl(url);
				if (previous !== undefined) {
					environment.moduleGraph.invalidateModule(previous);
				}
				const result = await environment.transformRequest(url);
				const module =
					await environment.moduleGraph.getModuleByUrl(url);
				return { result, module };
			},
			catch: (cause) => {
				return createDevStylesheetIssue({
					filePath: input.filePath,
					problem: describeCause(cause),
					cause,
				});
			},
		});
		if (transformed.result === null || transformed.module === undefined) {
			return yield* Effect.fail(
				createDevStylesheetIssue({
					filePath: input.filePath,
					problem: "Vite did not produce the stylesheet",
					cause: undefined,
				}),
			);
		}
		const visited = new Set<EnvironmentModuleNode>();
		const pending = [transformed.module];
		const files = new Set<string>();
		while (pending.length > 0) {
			const module = pending.pop();
			if (module === undefined || visited.has(module)) {
				continue;
			}
			visited.add(module);
			if (module.file !== null) {
				files.add(module.file);
			}
			pending.push(...module.importedModules);
		}
		const minified = minifyCriticalCss({ css: transformed.result.code });
		if (Result.isFailure(minified)) {
			return yield* Effect.fail(
				createDevStylesheetIssue({
					filePath: input.filePath,
					problem: describeCause(minified.failure),
					cause: minified.failure,
				}),
			);
		}
		return { css: minified.success, inputFilePaths: [...files] };
	});
}
