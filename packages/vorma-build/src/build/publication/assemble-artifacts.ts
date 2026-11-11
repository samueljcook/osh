import { Effect, Order, Result } from "effect";
import type { ClientBundle } from "../bundling/bundle-client.ts";
import type { ApplicationGeneration } from "../generation/generate-application.ts";
import { toClientBuildId, toContentHash } from "../output/content-hash.ts";
import { ARTIFACT_PROTOCOL } from "../output/dist-dir.ts";
import { toPublishedFileUrl } from "../output/published-file-url.ts";
import { projectBrowserContracts } from "./browser-contracts.ts";
import {
	BUILD_METADATA_FORMAT_VERSION,
	type BuildMetadata,
} from "./build-metadata.ts";

export type ApplicationArtifact = {
	/** Relative to the published public directory, with decoded filenames. */
	readonly fileName: string;
	readonly url: string;
	readonly hash: string;
};

/** The build metadata that the Rust server reads: the browser build's metadata and the hash of the declarations that the build directory is built for. */
export type ServerBuildMetadata = BuildMetadata & {
	/** The format of the metadata file, which the server checks. */
	readonly formatVersion: number;
	readonly declarationsHash: string;
	/** The development session's values for the server's Content-Security-Policy sources; absent from production metadata. */
	readonly devCsp?: {
		/** Where pages load browser modules, styles, images, and fonts. */
		readonly moduleServerOrigin: string;
		/** The module server's WebSocket origin. */
		readonly webSocketOrigin: string;
		/** The value of Vite's `html.cspNonce`, which `vorma dev` chooses unless the application sets it. */
		readonly nonce?: string;
	};
};

export type ApplicationArtifacts = {
	readonly metadata: ServerBuildMetadata;
	readonly files: ReadonlyArray<ApplicationArtifact>;
};

export type ArtifactAssemblyIssue = {
	readonly _tag: "ArtifactAssemblyIssue";
	readonly msg: string;
};

/** Joins published file metadata and checks paths, collisions, and module projections. */
export function assembleApplicationArtifacts(input: {
	readonly generation: ApplicationGeneration;
	readonly client: ClientBundle;
}): Effect.Effect<ApplicationArtifacts, ArtifactAssemblyIssue> {
	return Effect.gen(function* () {
		const { generation, client } = input;
		const byFile = new Map<string, ApplicationArtifact>();
		const byUrl = new Map<string, ApplicationArtifact>();
		const dirs = new Set<string>();
		/** Records one output, or the artifact already recorded for the same bytes at the same file. */
		function addFile(output: {
			readonly fileName: string;
			readonly url: string;
			readonly hash: string;
		}): Result.Result<ApplicationArtifact, string> {
			const parsed = checkArtifactFileName(output.fileName);
			if (Result.isFailure(parsed)) {
				return Result.fail(parsed.failure);
			}
			// Bundle validation checks content identities before assembly. The
			// prefix also identifies immutable files to CDN and reverse-proxy rules.
			if (
				!output.fileName.startsWith(
					ARTIFACT_PROTOCOL.publicOutputFileNamePrefix,
				)
			) {
				return Result.fail(
					`The build would write ${output.fileName} to the public output directory, but every file there must have a content-hashed ${ARTIFACT_PROTOCOL.publicOutputFileNamePrefix} name`,
				);
			}
			const hash = output.hash;
			const existing = byFile.get(output.fileName);
			if (existing !== undefined) {
				if (existing.hash !== hash || existing.url !== output.url) {
					return Result.fail(
						`Conflicting artifact outputs: ${output.fileName}`,
					);
				}
				return Result.succeed(existing);
			}
			if (byUrl.has(output.url)) {
				return Result.fail(`Conflicting artifact URL: ${output.url}`);
			}
			const parts = output.fileName.split("/");
			const parents = parts
				.slice(0, -1)
				.map((_part, index) => parts.slice(0, index + 1).join("/"));
			if (
				dirs.has(output.fileName) ||
				parents.some((parent) => byFile.has(parent))
			) {
				return Result.fail(
					`Artifact file and directory conflict: ${output.fileName}`,
				);
			}
			byFile.set(output.fileName, output);
			byUrl.set(output.url, output);
			for (const parent of parents) {
				dirs.add(parent);
			}
			return Result.succeed(output);
		}
		for (const asset of generation.staticPublicAssets.assets) {
			yield* Effect.fromResult(
				addFile({
					fileName: asset.fileName,
					url: asset.url,
					hash: asset.hash,
				}),
			);
		}
		for (const output of client.outputs) {
			const source =
				typeof output.source === "string"
					? new TextEncoder().encode(output.source)
					: output.source;
			yield* Effect.fromResult(
				addFile({
					fileName: output.fileName,
					hash: toContentHash(source),
					url: yield* Effect.fromResult(
						toPublishedFileUrl({
							staticPublicAssetsServeUrlPathPrefix:
								generation.sourceLayout
									.staticPublicAssetsServeUrlPathPrefix,
							fileName: output.fileName,
						}),
					),
				}),
			);
		}
		const staticPublicAssetUrls =
			generation.staticPublicAssets.staticPublicAssetUrls;
		const files = [...byFile.values()].sort(
			Order.mapInput(Order.String, (file) => file.url),
		);
		// What the browser's requests depend on, which the client build ID covers.
		const browserMetadata = {
			runtimeMode: "prod" as const,
			staticPublicAssetsServeUrlPathPrefix:
				generation.sourceLayout.staticPublicAssetsServeUrlPathPrefix,
			staticPublicAssetUrls,
			criticalCss: client.criticalCss,
			clientEntry: client.clientEntry,
			clientViews: client.clientViews,
		};
		return {
			files,
			metadata: {
				formatVersion: BUILD_METADATA_FORMAT_VERSION,
				runtimeMode: browserMetadata.runtimeMode,
				clientBuildId: yield* Effect.fromResult(
					Result.mapError(
						toClientBuildId({
							version: 3,
							metadata: browserMetadata,
							contracts: projectBrowserContracts(
								generation.inspection,
							),
							files: files.map((file) => {
								return { url: file.url, hash: file.hash };
							}),
						}),
						(issue) => issue.msg,
					),
				),
				declarationsHash: generation.inspection.declarationsHash,
				staticPublicAssetsServeUrlPathPrefix:
					browserMetadata.staticPublicAssetsServeUrlPathPrefix,
				staticPublicAssetFileNames: files.map((file) => file.fileName),
				staticPublicAssetUrls,
				criticalCss: browserMetadata.criticalCss,
				clientEntry: browserMetadata.clientEntry,
				clientViews: browserMetadata.clientViews,
			},
		};
	}).pipe(
		Effect.mapError((cause): ArtifactAssemblyIssue => {
			return {
				_tag: "ArtifactAssemblyIssue",
				msg: `Cannot assemble the build output: ${cause}`,
			};
		}),
	);
}

function checkArtifactFileName(
	fileName: string,
): Result.Result<string, string> {
	const parts = fileName.split("/");
	if (
		parts.some((part) => {
			return (
				part === "" ||
				part === "." ||
				part === ".." ||
				/[\\:\0]/.test(part)
			);
		})
	) {
		return Result.fail(`Invalid artifact output path: ${fileName}`);
	}
	return Result.succeed(fileName);
}
