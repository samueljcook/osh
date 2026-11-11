import { Effect, Record, Result } from "effect";
import * as FileSystem from "effect/FileSystem";
import * as Path from "effect/Path";
import { testPathContained } from "primitives-ts/fs/path-containment/node";
import { describeCause } from "../issues.ts";
import { canonicalizeOutputPath } from "../output/output-ownership.ts";
import type { ResolvedVormaConfig } from "./resolve-config.ts";

declare const APPLICATION_SOURCE_LAYOUT_BRAND: unique symbol;

export type ApplicationSourceLayout = ResolvedVormaConfig & {
	readonly [APPLICATION_SOURCE_LAYOUT_BRAND]: true;
};

/** The configuration key that names a path. */
export type ApplicationSourcePathField =
	| "dev.server.cargoManifestFile"
	| "client.tsConfigFile"
	| "client.viteConfigFile"
	| "client.entryFile"
	| "client.criticalCssFile"
	| "client.tsGenOutFile"
	| "core.staticPublicAssets.srcDir"
	| "core.distDir";

export type ApplicationSourceLayoutIssue = {
	readonly field: ApplicationSourcePathField;
	readonly path: string;
	readonly msg: string;
};

export type ApplicationSourceLayoutIssues = {
	readonly _tag: "ApplicationSourceLayoutIssues";
	readonly issues: ReadonlyArray<ApplicationSourceLayoutIssue>;
};

type SourcePathRequirement = {
	readonly field: ApplicationSourcePathField;
	readonly path: string;
	readonly expectedType: "File" | "Directory";
};

/** Loads the configured source layout before any application module executes. */
export function loadApplicationSourceLayout<Config extends ResolvedVormaConfig>(
	config: Config,
): Effect.Effect<
	ApplicationSourceLayout & Config,
	ApplicationSourceLayoutIssues,
	FileSystem.FileSystem | Path.Path
> {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const path = yield* Path.Path;
		const requiredPaths = {
			clientEntryFilePath: {
				field: "client.entryFile",
				path: config.clientEntryFilePath,
				expectedType: "File",
			},
			clientTsConfigFilePath: {
				field: "client.tsConfigFile",
				path: config.clientTsConfigFilePath,
				expectedType: "File",
			},
			staticPublicAssetsSourceDir: {
				field: "core.staticPublicAssets.srcDir",
				path: config.staticPublicAssetsSourceDir,
				expectedType: "Directory",
			},
			viteConfigFilePath: {
				field: "client.viteConfigFile",
				path: config.viteConfigFilePath,
				expectedType: "File",
			},
		} satisfies Record<string, SourcePathRequirement>;
		const criticalCssRequirement =
			config.criticalCssEntryFilePath === undefined
				? undefined
				: ({
						field: "client.criticalCssFile",
						path: config.criticalCssEntryFilePath,
						expectedType: "File",
					} satisfies SourcePathRequirement);
		const cargoManifestRequirement =
			config.dev === undefined
				? undefined
				: ({
						field: "dev.server.cargoManifestFile",
						path: config.dev.server.cargoManifestFilePath,
						expectedType: "File",
					} satisfies SourcePathRequirement);
		function toIssue(
			requirement: SourcePathRequirement,
			problem: string,
		): ApplicationSourceLayoutIssue {
			return {
				field: requirement.field,
				path: requirement.path,
				msg: `${requirement.field} names ${path.relative(config.configDir, requirement.path)}, ${problem}`,
			};
		}
		function inspectSourcePath(
			requirement: SourcePathRequirement,
		): Effect.Effect<Result.Result<string, ApplicationSourceLayoutIssue>> {
			return Effect.all({
				stat: Effect.result(fileSystem.stat(requirement.path)),
				canonicalPath: Effect.result(
					fileSystem.realPath(requirement.path),
				),
			}).pipe(
				Effect.map((inspection) => {
					if (Result.isFailure(inspection.stat)) {
						const failure = inspection.stat.failure;
						return Result.fail(
							toIssue(
								requirement,
								failure.reason._tag === "NotFound"
									? "which does not exist"
									: `which cannot be read: ${failure.message}`,
							),
						);
					}
					const actualType = inspection.stat.success.type;
					if (actualType !== requirement.expectedType) {
						return Result.fail(
							toIssue(
								requirement,
								`which is a ${actualType.toLowerCase()}, not a ${requirement.expectedType.toLowerCase()}`,
							),
						);
					}
					if (Result.isFailure(inspection.canonicalPath)) {
						return Result.fail(
							toIssue(
								requirement,
								`which cannot be resolved: ${inspection.canonicalPath.failure.message}`,
							),
						);
					}
					return Result.succeed(inspection.canonicalPath.success);
				}),
			);
		}
		const inspections = yield* Effect.all(
			{
				...Record.map(requiredPaths, inspectSourcePath),
				criticalCssEntryFilePath:
					criticalCssRequirement === undefined
						? Effect.succeed(Result.succeed(undefined))
						: inspectSourcePath(criticalCssRequirement),
				cargoManifestFilePath:
					cargoManifestRequirement === undefined
						? Effect.succeed(Result.succeed(undefined))
						: inspectSourcePath(cargoManifestRequirement),
			},
			{ concurrency: "unbounded" },
		);
		const issues = Object.values(inspections).flatMap((inspection) => {
			return Result.isFailure(inspection) ? [inspection.failure] : [];
		});
		const canonicalPaths = Result.all(inspections);
		if (Result.isFailure(canonicalPaths)) {
			return yield* Effect.fail({
				_tag: "ApplicationSourceLayoutIssues",
				issues,
			} satisfies ApplicationSourceLayoutIssues);
		}
		const {
			criticalCssEntryFilePath,
			cargoManifestFilePath,
			...canonical
		} = canonicalPaths.success;
		const canonicalConfig: ResolvedVormaConfig = {
			...config,
			...canonical,
			...(criticalCssEntryFilePath === undefined
				? {}
				: { criticalCssEntryFilePath }),
			...(config.dev === undefined || cargoManifestFilePath === undefined
				? {}
				: {
						dev: {
							...config.dev,
							server: {
								...config.dev.server,
								cargoManifestFilePath,
							},
						},
					}),
		};
		const publicSource = canonicalConfig.staticPublicAssetsSourceDir;
		if (
			criticalCssEntryFilePath !== undefined &&
			testPathContained({
				path,
				dir: publicSource,
				filePath: criticalCssEntryFilePath,
			})
		) {
			return yield* Effect.fail({
				_tag: "ApplicationSourceLayoutIssues",
				issues: [
					{
						field: "client.criticalCssFile",
						path: criticalCssEntryFilePath,
						msg: `client.criticalCssFile (${path.relative(config.configDir, criticalCssEntryFilePath)}) is inside core.staticPublicAssets.srcDir (${path.relative(config.configDir, publicSource)}); the build compiles the critical CSS file itself and publishes every file of core.staticPublicAssets.srcDir`,
					},
				],
			} satisfies ApplicationSourceLayoutIssues);
		}
		function canonicalizeOutput(requirement: SourcePathRequirement) {
			return canonicalizeOutputPath(requirement.path).pipe(
				Effect.mapError((cause): ApplicationSourceLayoutIssues => {
					return {
						_tag: "ApplicationSourceLayoutIssues",
						issues: [
							toIssue(
								requirement,
								`which cannot be resolved: ${describeCause(cause.cause)}`,
							),
						],
					};
				}),
			);
		}
		const outputs = yield* Effect.all(
			{
				distDir: canonicalizeOutput({
					field: "core.distDir",
					path: config.distDir,
					expectedType: "Directory",
				}),
				clientTypesFilePath: canonicalizeOutput({
					field: "client.tsGenOutFile",
					path: config.clientTypesFilePath,
					expectedType: "File",
				}),
			},
			{ concurrency: "unbounded" },
		);
		const { distDir, clientTypesFilePath } = outputs;
		const clientTypesOverlaps = [
			{ field: "core.distDir", dir: distDir },
			{ field: "core.staticPublicAssets.srcDir", dir: publicSource },
		].filter(({ dir }) => {
			return testPathContained({
				path,
				dir,
				filePath: clientTypesFilePath,
			});
		});
		if (clientTypesOverlaps.length > 0) {
			return yield* Effect.fail({
				_tag: "ApplicationSourceLayoutIssues",
				issues: clientTypesOverlaps.map(({ field, dir }) => {
					return {
						field: "client.tsGenOutFile",
						path: clientTypesFilePath,
						msg: `client.tsGenOutFile (${path.relative(config.configDir, clientTypesFilePath)}) is inside ${field} (${path.relative(config.configDir, dir)}); the build writes the generated types there, and a build would delete or publish them`,
					};
				}),
			} satisfies ApplicationSourceLayoutIssues);
		}
		if (
			testPathContained({
				path,
				dir: publicSource,
				filePath: distDir,
			}) ||
			testPathContained({
				path,
				dir: distDir,
				filePath: publicSource,
			})
		) {
			return yield* Effect.fail({
				_tag: "ApplicationSourceLayoutIssues",
				issues: [
					{
						field: "core.staticPublicAssets.srcDir",
						path: publicSource,
						msg: `core.staticPublicAssets.srcDir (${path.relative(config.configDir, publicSource)}) overlaps core.distDir (${path.relative(config.configDir, distDir)}); the build cannot replace its own input or capture its own output`,
					},
				],
			} satisfies ApplicationSourceLayoutIssues);
		}
		return canonicalConfig as ApplicationSourceLayout & Config;
	});
}
