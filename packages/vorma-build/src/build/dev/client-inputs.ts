import type { StaticPublicAssetProjection } from "../assets/static-public-assets.ts";
import type { ClientInspection } from "../inspection/inspect-projects.ts";
import type { DevLayout } from "./generated-paths.ts";

/** Inspected build information stays in the orchestrator, never in a browser manifest. */
export type DevClientInputs = ReturnType<typeof toDevClientInputs>;

/**
 * What Vite needs from one generation. It reads only the client's inspection
 * and the static public assets, so Vite can start before the server's
 * inspection ends.
 */
export function toDevClientInputs(input: {
	readonly layout: DevLayout;
	readonly inspection: Pick<
		ClientInspection,
		"clientViews" | "staticPublicAssetLookups" | "clientSourceTexts"
	>;
	readonly staticPublicAssets: StaticPublicAssetProjection;
	/** The client project's configuration and resolution files that are not program source files. */
	readonly resolutionFiles: ReadonlySet<string>;
}) {
	const { layout, inspection, staticPublicAssets } = input;
	return {
		configFilePath: layout.configFilePath,
		viteConfigFilePath: layout.viteConfigFilePath,
		clientEntryFilePath: layout.clientEntryFilePath,
		/** Every view file, in path order. */
		clientViewModules: [
			...new Set(inspection.clientViews.map((view) => view.filePath)),
		].sort(),
		coordinatedResolutionFiles: input.resolutionFiles,
		generatedPaths: layout.generatedPaths,
		staticPublicAssetLookups: inspection.staticPublicAssetLookups,
		clientSourceTexts: Object.fromEntries(inspection.clientSourceTexts),
		staticPublicAssets: staticPublicAssets.assets,
		staticPublicAssetUrls: staticPublicAssets.staticPublicAssetUrls,
	};
}
