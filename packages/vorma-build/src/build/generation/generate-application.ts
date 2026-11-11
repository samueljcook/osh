import { Effect, Result } from "effect";
import * as FileSystem from "effect/FileSystem";
import * as Path from "effect/Path";
import type { ChildProcessSpawner } from "effect/process";
import {
	collectStaticPublicAssets,
	type StaticPublicAssetIssue,
	type StaticPublicAssetProjection,
} from "../assets/static-public-assets.ts";
import type { ConfigResolutionIssue } from "../config/resolve-config.ts";
import type {
	ApplicationSourceLayout,
	ApplicationSourceLayoutIssues,
} from "../config/source-layout.ts";
import { withApplicationInspection } from "../inspection/inspect-application.ts";
import type {
	InspectedProjects,
	InspectionIssues,
} from "../inspection/inspect-projects.ts";
import { inspectStartedRustServer } from "../inspection/rust-server.ts";
import type { TsProjectIssue } from "../inspection/typescript-projects.ts";
import { writeOutputFile } from "../output/output-files.ts";
import {
	compileApplicationModel,
	type ApplicationModel,
	type ApplicationModelIssue,
} from "./compile-application.ts";
import { renderGeneratedClientTypes } from "./render-client-contracts.ts";
import type { GeneratedSource } from "./render-source-text.ts";

export type ApplicationModelIssues = {
	readonly _tag: "ApplicationModelIssues";
	readonly issues: ReadonlyArray<ApplicationModelIssue>;
};

/** The `staticPublicAssets` lookups that name assets that the static public asset directory does not contain. */
export type UnknownStaticPublicAssetKeys = {
	readonly _tag: "UnknownStaticPublicAssetKeys";
	readonly issues: ReadonlyArray<{ readonly msg: string }>;
};

export type GeneratedApplicationWriteIssue = {
	readonly _tag: "GeneratedApplicationWriteIssue";
	readonly filePath: string;
	readonly msg: string;
	readonly cause: unknown;
};

export type ApplicationGenerationIssue =
	| ConfigResolutionIssue
	| StaticPublicAssetIssue
	| TsProjectIssue
	| InspectionIssues
	| ApplicationSourceLayoutIssues
	| ApplicationModelIssues
	| UnknownStaticPublicAssetKeys;

export type ApplicationGeneration = {
	readonly sourceLayout: ApplicationSourceLayout;
	readonly model: ApplicationModel;
	readonly inspection: InspectedProjects;
	/** The client project's generated types, at the configured `client.tsGenOutFile`. */
	readonly clientTypes: GeneratedSource;
	readonly staticPublicAssets: StaticPublicAssetProjection;
};

/** Reads the server's contract and the client project, captures static public assets, and prepares generated build inputs without publishing them. */
export function prepareApplicationGeneration(
	sourceLayout: ApplicationSourceLayout,
): Effect.Effect<
	ApplicationGeneration,
	ApplicationGenerationIssue,
	FileSystem.FileSystem | Path.Path | ChildProcessSpawner.ChildProcessSpawner
> {
	return Effect.gen(function* () {
		const read = yield* Effect.all(
			{
				inspection: inspectApplicationSources(sourceLayout),
				staticPublicAssets: collectStaticPublicAssets(sourceLayout),
			},
			{ concurrency: "unbounded" },
		);
		return yield* assembleApplicationGeneration({ sourceLayout, ...read });
	});
}

/** Reads declarations and static public asset lookups independently of static public asset hashing. */
export function inspectApplicationSources(
	sourceLayout: ApplicationSourceLayout,
) {
	return withApplicationInspection({
		layout: sourceLayout,
		readServer: inspectStartedRustServer({
			cmd: sourceLayout.serverStartCmd,
			configDir: sourceLayout.configDir,
		}),
		onClientInspectedFn: () => Effect.void,
		useFn: ({ inspection }) => Effect.succeed(inspection),
	});
}

/** Prepares correlated build inputs from an inspection and a captured public directory. */
export function assembleApplicationGeneration(input: {
	readonly sourceLayout: ApplicationSourceLayout;
	readonly inspection: InspectedProjects;
	readonly staticPublicAssets: StaticPublicAssetProjection;
}): Effect.Effect<
	ApplicationGeneration,
	ApplicationModelIssues | UnknownStaticPublicAssetKeys,
	Path.Path
> {
	return Effect.gen(function* () {
		const path = yield* Path.Path;
		const { sourceLayout, inspection, staticPublicAssets } = input;
		const model = compileApplicationModel(inspection);
		if (Result.isFailure(model)) {
			return yield* Effect.fail({
				_tag: "ApplicationModelIssues",
				issues: model.failure,
			} satisfies ApplicationModelIssues);
		}
		const { staticPublicAssetUrls } = staticPublicAssets;
		const unknownLookups = inspection.staticPublicAssetLookups.filter(
			(lookup) => {
				return !Object.hasOwn(staticPublicAssetUrls, lookup.key);
			},
		);
		if (unknownLookups.length > 0) {
			return yield* Effect.fail({
				_tag: "UnknownStaticPublicAssetKeys",
				issues: unknownLookups.map((lookup) => {
					return {
						msg: `${lookup.location}: staticPublicAssets names ${JSON.stringify(lookup.key)}, which is not the key of a file of core.staticPublicAssets.srcDir; a key is the file's exact path relative to that directory, with "/" between its parts and no leading "/"`,
					};
				}),
			} satisfies UnknownStaticPublicAssetKeys);
		}
		return {
			sourceLayout,
			model: model.success,
			inspection,
			staticPublicAssets,
			clientTypes: renderGeneratedClientTypes({
				model: model.success,
				inspection,
				staticPublicAssets,
				filePath: sourceLayout.clientTypesFilePath,
				path,
			}),
		};
	});
}

/** Writes the client's generated types file, replacing it atomically. */
export function writeApplicationGeneration(
	generation: ApplicationGeneration,
): Effect.Effect<
	void,
	GeneratedApplicationWriteIssue,
	FileSystem.FileSystem | Path.Path
> {
	const output = generation.clientTypes;
	return writeOutputFile(output).pipe(
		Effect.mapError((cause): GeneratedApplicationWriteIssue => {
			return {
				_tag: "GeneratedApplicationWriteIssue",
				filePath: output.filePath,
				msg:
					cause._tag === "PlatformError"
						? `Could not write ${output.filePath}: ${cause.message}`
						: cause.msg,
				cause,
			};
		}),
	);
}
