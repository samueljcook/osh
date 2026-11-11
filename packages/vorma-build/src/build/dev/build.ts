import { Effect } from "effect";
import type { startDevClient } from "./client.ts";
import type { DevGeneration } from "./generation.ts";
import { readDevStylesheet } from "./stylesheet.ts";
import type { DevWork } from "./work.ts";

export type DevClient = Effect.Success<ReturnType<typeof startDevClient>>;

export type DevBuild = {
	readonly generation: DevGeneration;
	readonly stylesheet: {
		readonly css: string;
		readonly inputFilePaths: ReadonlyArray<string>;
	};
};

/**
 * Prepares native CSS through Vite after Vite has the generation's inputs. A
 * later build reads the critical CSS again unless its work is client work
 * alone; work that restarts Vite for a file of Vite's configuration or a
 * resolution file is not client work alone.
 */
export function prepareDevBuild(input: {
	readonly generation: DevGeneration;
	readonly client: DevClient;
	readonly previous?: {
		readonly build: DevBuild;
		readonly work: DevWork;
	};
}) {
	return Effect.gen(function* () {
		const { generation, client, previous } = input;
		if (
			previous !== undefined &&
			!previous.work.serverWork &&
			previous.work.staticWork === undefined &&
			!previous.work.viteRestart
		) {
			return {
				generation,
				stylesheet: previous.build.stylesheet,
			} satisfies DevBuild;
		}
		const filePath =
			generation.application.sourceLayout.criticalCssEntryFilePath;
		const stylesheet =
			filePath === undefined
				? { css: "", inputFilePaths: [] }
				: yield* readDevStylesheet({ server: client.server, filePath });
		return { generation, stylesheet } satisfies DevBuild;
	});
}
