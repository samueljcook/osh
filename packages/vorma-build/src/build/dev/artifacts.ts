import { Result } from "effect";
import { normalizePath } from "vite";
import { getClientViewEntries } from "../generation/client-view-entries.ts";
import { toClientBuildId } from "../output/content-hash.ts";
import type { ServerBuildMetadata } from "../publication/assemble-artifacts.ts";
import { projectBrowserContracts } from "../publication/browser-contracts.ts";
import {
	BUILD_METADATA_FORMAT_VERSION,
	type ClientModule,
} from "../publication/build-metadata.ts";
import type { DevBuild, DevClient } from "./build.ts";
import type { IndexHtmlParts } from "./index-html.ts";

/**
 * The decoded URL path at which Vite serves a module id, as Vite's own import
 * URLs spell it: the path below Vite's root for a file inside the root, `/@fs/`
 * and the absolute path for any other file, and `/@id/__x00__` and the id for
 * a virtual module. Vite's `transformRequest` takes this form.
 */
export function toDevModulePath(input: {
	readonly root: string;
	readonly id: string;
}) {
	const id = normalizePath(input.id);
	const root = `${normalizePath(input.root).replace(/\/$/, "")}/`;
	return id.startsWith("\0")
		? `/@id/__x00__${id.slice(1)}`
		: id.startsWith(root)
			? `/${id.slice(root.length)}`
			: `/@fs/${id.replace(/^\//, "")}`;
}

/** The module server URL of a module id; it matches Vite's import URLs so that a physical file has one browser module identity. */
export function toDevModuleUrl(input: {
	readonly origin: string;
	readonly root: string;
	readonly id: string;
}) {
	return (
		input.origin +
		toDevModulePath(input)
			.split("/")
			.map(encodeURIComponent)
			.join("/")
			.replaceAll("%40", "@")
			.replaceAll("%3A", ":")
	);
}

/** Keeps source URLs and browser contracts on the server; responses project only requested views. */
export function assembleDevArtifacts(input: {
	readonly build: DevBuild;
	readonly client: DevClient;
	readonly indexHtmlParts: IndexHtmlParts;
}) {
	const generation = input.build.generation.application;
	function toModule(id: string): ClientModule {
		return {
			importUrl: toDevModuleUrl({
				origin: input.client.origin,
				root: input.client.server.config.root,
				id,
			}),
		};
	}
	const clientViews = Object.fromEntries(
		getClientViewEntries(generation).map((view) => {
			return [view.pattern, toModule(view.id)];
		}),
	);
	const { headHtml, bodyPrependHtml, bodyHtml } = input.indexHtmlParts;
	return Result.map(
		toClientBuildId({
			contracts: projectBrowserContracts(generation.inspection),
			views: clientViews,
			staticPublicAssetUrls:
				generation.staticPublicAssets.staticPublicAssetUrls,
		}),
		(clientBuildId) => {
			return {
				metadata: {
					formatVersion: BUILD_METADATA_FORMAT_VERSION,
					runtimeMode: "dev",
					clientBuildId,
					declarationsHash: generation.inspection.declarationsHash,
					staticPublicAssetsServeUrlPathPrefix:
						generation.sourceLayout
							.staticPublicAssetsServeUrlPathPrefix,
					staticPublicAssetFileNames:
						generation.staticPublicAssets.assets.map(
							(asset) => asset.fileName,
						),
					staticPublicAssetUrls:
						generation.staticPublicAssets.staticPublicAssetUrls,
					criticalCss: input.build.stylesheet.css,
					devCsp: {
						moduleServerOrigin: input.client.origin,
						webSocketOrigin: input.client.webSocketOrigin,
						...(input.client.cspNonce === undefined
							? {}
							: { nonce: input.client.cspNonce }),
					},
					// The metadata leaves out a part of the development HTML that is empty.
					...(headHtml === "" ? {} : { devHeadHtml: headHtml }),
					...(bodyPrependHtml === ""
						? {}
						: { devBodyPrependHtml: bodyPrependHtml }),
					...(bodyHtml === "" ? {} : { devBodyHtml: bodyHtml }),
					clientEntry: toModule(
						generation.sourceLayout.clientEntryFilePath,
					),
					clientViews,
				} satisfies ServerBuildMetadata,
				files: generation.staticPublicAssets.assets,
			};
		},
	);
}
