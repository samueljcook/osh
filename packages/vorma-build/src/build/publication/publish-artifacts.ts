import { Effect } from "effect";
import * as FileSystem from "effect/FileSystem";
import * as Path from "effect/Path";
import { writeFileAtomically } from "primitives-ts/fs/atomic-writes/node";
import type { ApplicationSourceLayout } from "../config/source-layout.ts";
import type { ApplicationGeneration } from "../generation/generate-application.ts";
import { encodeContentHash, hashFile } from "../output/content-hash.ts";
import { describeAtomicWriteError } from "../output/output-files.ts";
import {
	checkOutputPath,
	type WrittenBuildMetadata,
} from "../output/output-ownership.ts";
import type { ApplicationArtifacts } from "./assemble-artifacts.ts";

export type ArtifactPublicationIssue = {
	readonly _tag: "ArtifactPublicationIssue";
	readonly msg: string;
	readonly cause: unknown;
};

/** Empties the public directory of `vorma dev` before a session adds its files. */
export function preparePublicOutput(
	sourceLayout: Pick<ApplicationSourceLayout, "publicDir" | "buildDir">,
) {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const dir = sourceLayout.publicDir;
		yield* checkOutputPath({
			filePath: dir,
			rootDir: sourceLayout.buildDir,
		});
		yield* fileSystem.remove(dir, { force: true, recursive: true }).pipe(
			Effect.andThen(fileSystem.makeDirectory(dir, { recursive: true })),
			Effect.mapError((cause): ArtifactPublicationIssue => {
				return {
					_tag: "ArtifactPublicationIssue",
					msg: `Cannot prepare the public directory ${dir}: ${cause.message}`,
					cause,
				};
			}),
		);
	});
}

/**
 * Writes the build metadata, which the server reads, for the completed browser
 * build. The file is replaced atomically, because `vorma dev` writes it in the
 * build directory that its server reads.
 */
export function writeBuildMetadata(input: {
	readonly artifacts: ApplicationArtifacts;
	readonly generation: ApplicationGeneration;
}) {
	return Effect.gen(function* () {
		const path = yield* Path.Path;
		yield* Effect.forEach(
			input.artifacts.files,
			(file) => {
				return Effect.gen(function* () {
					const filePath = path.join(
						input.generation.sourceLayout.publicDir,
						file.fileName,
					);
					yield* checkOutputPath({
						filePath,
						rootDir: input.generation.sourceLayout.publicDir,
					}).pipe(
						Effect.mapError((cause): ArtifactPublicationIssue => {
							return {
								_tag: "ArtifactPublicationIssue",
								msg: cause.msg,
								cause,
							};
						}),
					);
					const actualHash = encodeContentHash(
						yield* hashFile(filePath),
					);
					if (actualHash !== file.hash) {
						return yield* Effect.fail({
							_tag: "ArtifactPublicationIssue",
							cause: undefined,
							msg: `Cannot publish ${file.fileName}: its written bytes differ from the build output. Plugins must finish content changes before the bundler calculates output hashes.`,
						} satisfies ArtifactPublicationIssue);
					}
				});
			},
			{ concurrency: 16, discard: true },
		);
		const metadataPath =
			input.generation.sourceLayout.buildMetadataFilePath;
		const written = yield* Effect.promise(() => {
			return writeFileAtomically({
				filePath: metadataPath,
				bytes: new TextEncoder().encode(
					`${JSON.stringify(input.artifacts.metadata, null, "\t")}\n`,
				),
				permissions: { kind: "platform-default" },
				replacement: "always",
				durability: "none",
			});
		});
		if (written.kind === "error") {
			return yield* Effect.fail({
				_tag: "ArtifactPublicationIssue",
				msg: `Cannot write the build metadata ${metadataPath}: ${describeAtomicWriteError(written.error)}`,
				cause: written.error,
			} satisfies ArtifactPublicationIssue);
		}
		return {
			_tag: "WrittenBuildMetadata",
			filePath: metadataPath,
			staticPublicAssetFileNames:
				input.artifacts.metadata.staticPublicAssetFileNames,
		} satisfies WrittenBuildMetadata;
	}).pipe(
		Effect.mapError((cause): ArtifactPublicationIssue => {
			if (cause._tag === "ArtifactPublicationIssue") {
				return cause;
			}
			return {
				_tag: "ArtifactPublicationIssue",
				msg: `Cannot write the build metadata ${input.generation.sourceLayout.buildMetadataFilePath}: ${cause.message}`,
				cause,
			};
		}),
	);
}
