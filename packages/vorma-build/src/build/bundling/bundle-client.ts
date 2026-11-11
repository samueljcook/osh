import { stripVTControlCharacters } from "node:util";
import { Effect, Result } from "effect";
import * as FileSystem from "effect/FileSystem";
import * as Path from "effect/Path";
import { testPathContained } from "primitives-ts/fs/path-containment/node";
import { build, normalizePath, type Plugin, type Rollup } from "vite";
import { checkVormaPlugin } from "../../vite/plugin-api.ts";
import {
	createCssStaticPublicAssets,
	type CssStaticPublicAssets,
	type JsAssetReference,
} from "../assets/css-static-public-assets.ts";
import { createStaticPublicAssetPlugin } from "../assets/static-public-asset-plugin.ts";
import { createViteConfigCheckPlugin } from "../config/load-config.ts";
import type { StaticPublicAssetsServeUrlPathPrefix } from "../config/resolve-config.ts";
import {
	createBuildViteOptions,
	createClientBuildViteOptions,
} from "../config/vite-options.ts";
import {
	getClientViewEntries,
	type ClientViewEntry,
} from "../generation/client-view-entries.ts";
import type { ApplicationGeneration } from "../generation/generate-application.ts";
import { describeCause } from "../issues.ts";
import { toContentHash } from "../output/content-hash.ts";
import { ARTIFACT_PROTOCOL } from "../output/dist-dir.ts";
import { checkOutputPath } from "../output/output-ownership.ts";
import {
	toPublishedFileUrl,
	toRenderedAssetUrl,
} from "../output/published-file-url.ts";
import {
	createClientModule,
	type ClientModule,
} from "../publication/build-metadata.ts";
import { createClientOutputHashingPlugins } from "./client-output-hashing.ts";
import { findCriticalCssFileName } from "./critical-css.ts";

type ClientModuleOutput = {
	readonly file: string;
	readonly imports: ReadonlyArray<string>;
	readonly css: ReadonlyArray<string>;
	readonly assets: ReadonlyArray<string>;
	readonly assetUrls: ReadonlyArray<string>;
};

/**
 * The ids of the modules that Vorma names. Rolldown identifies a module by
 * the form that Vite's `normalizePath` gives its path, so every comparison
 * with a module id uses these.
 */
type EntryModuleIds = {
	readonly clientEntry: string;
	readonly criticalCssEntry: string | undefined;
};

export function getEntryModuleIds(
	generation: ApplicationGeneration,
): EntryModuleIds {
	const { clientEntryFilePath, criticalCssEntryFilePath } =
		generation.sourceLayout;
	return {
		clientEntry: normalizePath(clientEntryFilePath),
		criticalCssEntry:
			criticalCssEntryFilePath === undefined
				? undefined
				: normalizePath(criticalCssEntryFilePath),
	};
}

export type ClientBundleIssue = {
	readonly _tag: "ClientBundleIssue";
	readonly msg: string;
	readonly cause: unknown;
};

export type ClientBundle = {
	readonly criticalCss: string;
	readonly outputs: ReadonlyArray<{
		readonly fileName: string;
		readonly source: string | Uint8Array;
	}>;
	readonly chunks: ReadonlyArray<{
		readonly fileName: string;
		readonly sourceModules: ReadonlyArray<string>;
		/** Each source module's rendered length in the chunk before minification, for byte attribution. */
		readonly renderedLengths: Readonly<Record<string, number>>;
		readonly imports: ReadonlyArray<string>;
		readonly dynamicImports: ReadonlyArray<string>;
	}>;
	readonly clientEntry: ClientModule;
	readonly clientViews: Readonly<Record<string, ClientModule>>;
};

/** Builds browser entries directly into the build directory's prepared public directory. */
export function bundleClient(
	generation: ApplicationGeneration,
): Effect.Effect<
	ClientBundle,
	ClientBundleIssue,
	FileSystem.FileSystem | Path.Path
> {
	return Effect.gen(function* () {
		const path = yield* Path.Path;
		const viewEntries = getClientViewEntries(generation);
		const entryModuleIds = getEntryModuleIds(generation);
		const publicCss = createCssStaticPublicAssets(
			generation.staticPublicAssets,
		);
		// Vite cannot cancel this Promise; its output writes must finish before the build scope closes.
		const built = yield* Effect.tryPromise({
			try: () => {
				return runClientBuild({
					generation,
					publicCss,
					viewEntries,
					entryModuleIds,
				});
			},
			// Vite's own error describes what failed in the application's sources.
			catch: (cause) => cause,
		}).pipe(Effect.uninterruptible);
		if (built.kind !== "captured") {
			return yield* Effect.fail(
				"The Vite application build did not produce Vorma's client environment",
			);
		}
		const { criticalCss, manifestFiles, droppedFiles } = built;
		const { jsAssets } = publicCss;
		const fileSystem = yield* FileSystem.FileSystem;
		const { publicDir } = generation.sourceLayout;
		yield* Effect.forEach(
			manifestFiles,
			(fileName) => {
				const filePath = path.join(publicDir, fileName);
				return checkOutputPath({
					filePath,
					rootDir: publicDir,
				}).pipe(
					Effect.andThen(
						fileSystem.remove(filePath, { force: true }),
					),
					Effect.andThen(
						removeEmptyParentDirs({ filePath, rootDir: publicDir }),
					),
				);
			},
			{ concurrency: 1, discard: true },
		);
		// Build manifests are private metadata, not deployable browser assets.
		const outputs = Object.values(built.bundle).filter((output) => {
			return (
				!droppedFiles.has(output.fileName) &&
				!manifestFiles.has(output.fileName)
			);
		});
		const sourceMaps = new Set<string>();
		for (const output of outputs) {
			if (output.type !== "chunk") {
				continue;
			}
			if (output.preliminaryFileName === output.fileName) {
				return yield* Effect.fail(
					`The build would publish ${output.fileName} without a native content hash. Emit a normal chunk with Vorma's naming pattern instead of a fixed-name prebuilt chunk.`,
				);
			}
			const codeHash = toContentHash(output.code);
			if (
				built.preGenerateBundleChunkHashes.get(output.fileName) !==
					codeHash &&
				!output.fileName.includes(codeHash)
			) {
				return yield* Effect.fail(
					`The build would publish ${output.fileName} after a generateBundle hook modified its code without updating its content hash. Transform chunk code in renderChunk so the bundler includes the change in the chunk's hash, or include the chunk's complete base32 BLAKE3 hash in its fileName.`,
				);
			}
			if (output.sourcemapFileName !== null) {
				sourceMaps.add(output.sourcemapFileName);
			}
		}
		for (const output of outputs) {
			if (
				output.type === "asset" &&
				!sourceMaps.has(output.fileName) &&
				built.nativeOutputHashes.get(output.fileName) !==
					toContentHash(output.source) &&
				!output.fileName.includes(toContentHash(output.source))
			) {
				return yield* Effect.fail(
					`The build would publish ${output.fileName} without its BLAKE3 content hash. A plugin should emit the asset with a name and let the bundler choose its content-hashed ${ARTIFACT_PROTOCOL.publicOutputFileNamePrefix} fileName, or include the asset's complete base32 BLAKE3 hash in an explicit fileName.`,
				);
			}
		}
		const chunkFiles = new Set(
			outputs.flatMap((output) => {
				return output.type === "chunk" ? [output.fileName] : [];
			}),
		);
		const modules = new Map<string, ClientModuleOutput>();
		for (const output of outputs) {
			if (output.type !== "chunk") {
				continue;
			}
			const assetUrls: Array<string> = [];
			for (const hostId of new Set([
				output.preliminaryFileName,
				output.fileName,
			])) {
				for (const reference of jsAssets.get(hostId) ?? []) {
					// URL hooks also run for dynamic preload lists; only retained asset imports belong here.
					if (
						!output.viteMetadata?.importedAssets.has(
							reference.fileName.replace(/[?#].*$/s, ""),
						)
					) {
						continue;
					}
					const url = yield* Effect.fromResult(
						toJsAssetUrl({
							reference,
							staticPublicAssetsServeUrlPathPrefix:
								generation.sourceLayout
									.staticPublicAssetsServeUrlPathPrefix,
						}),
					);
					if (url !== undefined) {
						assetUrls.push(url);
					}
				}
			}
			modules.set(output.fileName, {
				file: output.fileName,
				imports: output.imports.filter((file) => chunkFiles.has(file)),
				css: [...(output.viteMetadata?.importedCss ?? [])],
				assets: [...(output.viteMetadata?.importedAssets ?? [])],
				assetUrls,
			});
		}
		// A static public asset that a chunk references is already published;
		// the bundle contains no output for it.
		const files = new Set([
			...outputs.map((output) => output.fileName),
			...generation.staticPublicAssets.assets.map(
				(asset) => asset.fileName,
			),
		]);
		for (const [key, entry] of modules) {
			for (const file of [entry.file, ...entry.css, ...entry.assets]) {
				if (!files.has(file)) {
					return yield* Effect.fail(
						`Client module ${key} references a missing output: ${file}`,
					);
				}
			}
		}
		const frameworkEntries = new Set([
			entryModuleIds.clientEntry,
			...viewEntries.map((entry) => entry.id),
		]);
		const entries = new Map<string, ClientModule>();
		for (const output of outputs) {
			if (
				output.type !== "chunk" ||
				!output.isEntry ||
				output.facadeModuleId === null ||
				!frameworkEntries.has(output.facadeModuleId)
			) {
				continue;
			}
			entries.set(
				output.facadeModuleId,
				yield* Effect.fromResult(
					projectClientModule({
						modules,
						key: output.fileName,
						staticPublicAssetsServeUrlPathPrefix:
							generation.sourceLayout
								.staticPublicAssetsServeUrlPathPrefix,
					}),
				),
			);
		}
		const clientEntry = entries.get(entryModuleIds.clientEntry);
		if (clientEntry === undefined) {
			return yield* Effect.fail(
				"Vite did not preserve the configured client entry",
			);
		}
		const clientViews = yield* Effect.forEach(viewEntries, (view) => {
			const module = entries.get(view.id);
			return module === undefined
				? Effect.fail(
						`Vite did not preserve the view module ${view.id}`,
					)
				: Effect.succeed([view.pattern, module] as const);
		}).pipe(Effect.map(Object.fromEntries));
		return {
			criticalCss,
			outputs: outputs.map((output) => {
				return {
					fileName: output.fileName,
					source:
						output.type === "chunk"
							? output.code
							: typeof output.source === "string"
								? output.source
								: Uint8Array.from(output.source),
				};
			}),
			chunks: outputs.flatMap((output) => {
				return output.type === "chunk"
					? [
							{
								fileName: output.fileName,
								sourceModules: Object.keys(output.modules),
								renderedLengths: Object.fromEntries(
									Object.entries(output.modules).map(
										([id, module]) => {
											return [id, module.renderedLength];
										},
									),
								),
								imports: [...output.imports],
								dynamicImports: [...output.dynamicImports],
							},
						]
					: [];
			}),
			clientEntry,
			clientViews,
		};
	}).pipe(
		Effect.mapError((cause): ClientBundleIssue => {
			return {
				_tag: "ClientBundleIssue",
				// Rolldown colors its messages for a terminal; an issue's message is plain text.
				msg: `The client build failed: ${stripVTControlCharacters(describeCause(cause))}`,
				cause,
			};
		}),
	);
}

/**
 * Removes the directories between a removed file and `rootDir` that the
 * removal left empty, such as the `.vite` directory of a build manifest.
 */
function removeEmptyParentDirs(input: {
	readonly filePath: string;
	readonly rootDir: string;
}) {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const path = yield* Path.Path;
		for (
			let dir = path.dirname(input.filePath);
			dir !== input.rootDir &&
			testPathContained({ path, dir: input.rootDir, filePath: dir });
			dir = path.dirname(dir)
		) {
			const entries = yield* fileSystem.readDirectory(dir).pipe(
				Effect.catchReason("PlatformError", "NotFound", () => {
					return Effect.succeed<ReadonlyArray<string>>([]);
				}),
			);
			if (entries.length > 0) {
				return;
			}
			yield* fileSystem.remove(dir, { recursive: true, force: true });
		}
	});
}

/** The entry's module and, dependencies first, every static import that it reaches with their stylesheets. */
function projectClientModule(input: {
	readonly modules: ReadonlyMap<string, ClientModuleOutput>;
	readonly key: string;
	readonly staticPublicAssetsServeUrlPathPrefix: StaticPublicAssetsServeUrlPathPrefix;
}): Result.Result<ClientModule, string> {
	const root = input.modules.get(input.key);
	if (root === undefined) {
		return Result.fail(`Client output has no module: ${input.key}`);
	}
	const visited = new Set<string>();
	const dependencies = new Set<string>();
	const css = new Set<string>();
	const assets = new Set<string>();
	type Visit =
		| { readonly kind: "visit"; readonly key: string }
		| { readonly kind: "collect"; readonly key: string };
	const pending: Array<Visit> = [{ kind: "visit", key: input.key }];
	for (let next = pending.pop(); next !== undefined; next = pending.pop()) {
		const entry = input.modules.get(next.key);
		if (entry === undefined) {
			return Result.fail(`Client output has no module: ${next.key}`);
		}
		if (next.kind === "collect") {
			if (next.key !== input.key) {
				dependencies.add(entry.file);
			}
			for (const file of entry.css) {
				css.add(file);
			}
			for (const url of entry.assetUrls) {
				assets.add(url);
			}
			continue;
		}
		if (visited.has(next.key)) {
			continue;
		}
		visited.add(next.key);
		pending.push({ kind: "collect", key: next.key });
		const imports = entry.imports;
		for (const dependency of [...imports].reverse()) {
			pending.push({ kind: "visit", key: dependency });
		}
	}
	function toUrl(fileName: string): Result.Result<string, string> {
		return toPublishedFileUrl({
			staticPublicAssetsServeUrlPathPrefix:
				input.staticPublicAssetsServeUrlPathPrefix,
			fileName,
		});
	}
	return Result.gen(function* () {
		return createClientModule({
			importUrl: yield* toUrl(root.file),
			depUrls: yield* Result.all([...dependencies].map(toUrl)),
			cssBundleUrls: yield* Result.all([...css].map(toUrl)),
			referencedStaticPublicAssetUrls: [...assets],
		});
	});
}

/**
 * The URL that Vite wrote for an asset that JavaScript references: the URL
 * that the application's `experimental.renderBuiltUrl` returned, or the
 * published file's URL, with the query and the fragment of the reference.
 */
function toJsAssetUrl(input: {
	readonly reference: JsAssetReference;
	readonly staticPublicAssetsServeUrlPathPrefix: StaticPublicAssetsServeUrlPathPrefix;
}): Result.Result<string | undefined, string> {
	const { fileName, renderedUrl } = input.reference;
	if (typeof renderedUrl === "object" && renderedUrl.runtime) {
		return Result.succeed(undefined);
	}
	// The native relative form resolves from the chunk to the same published asset.
	if (typeof renderedUrl === "string" && renderedUrl.length > 0) {
		return renderedUrl.startsWith("data:")
			? Result.succeed(undefined)
			: toRenderedAssetUrl(renderedUrl);
	}
	if (fileName.startsWith("data:")) {
		return Result.succeed(undefined);
	}
	if (!fileName.isWellFormed()) {
		return Result.fail(
			`The build would publish ${JSON.stringify(fileName)}, which contains a lone UTF-16 surrogate that cannot be encoded in a URL.`,
		);
	}
	const suffixIndex = fileName.search(/[?#]/);
	return Result.map(
		toPublishedFileUrl({
			staticPublicAssetsServeUrlPathPrefix:
				input.staticPublicAssetsServeUrlPathPrefix,
			fileName:
				suffixIndex < 0 ? fileName : fileName.slice(0, suffixIndex),
		}),
		(url) => (suffixIndex < 0 ? url : url + fileName.slice(suffixIndex)),
	);
}

/** The client build's outcome: the bundle, once Vorma's plugins captured it. */
type BuildState =
	| { readonly kind: "awaiting-css" }
	| {
			readonly kind: "awaiting-output";
			readonly criticalCssFileName: string | undefined;
	  }
	| {
			readonly kind: "captured";
			readonly bundle: Rollup.OutputBundle;
			readonly nativeOutputHashes: ReadonlyMap<string, string>;
			readonly preGenerateBundleChunkHashes: ReadonlyMap<string, string>;
			/** The critical stylesheet as Vite's CSS pipeline produced it. */
			readonly criticalCss: string;
			/** The build manifests that the application's Vite configuration requested. */
			readonly manifestFiles: ReadonlySet<string>;
			/** The outputs that the capture removed from the bundle and that Rolldown's bundle object still lists. */
			readonly droppedFiles: ReadonlySet<string>;
	  };

async function runClientBuild(input: {
	readonly generation: ApplicationGeneration;
	readonly publicCss: CssStaticPublicAssets;
	readonly viewEntries: ReadonlyArray<ClientViewEntry>;
	readonly entryModuleIds: EntryModuleIds;
}): Promise<BuildState> {
	const { generation, publicCss, viewEntries, entryModuleIds } = input;
	const nativeOutputHashes = new Map<string, string>();
	const preGenerateBundleChunkHashes = new Map<string, string>();
	let buildState: BuildState = { kind: "awaiting-css" };
	function getBuildState(): BuildState {
		return buildState;
	}
	// Rolldown names each entry's output after its file, so adding a view renames
	// no other output. The build finds each output by its source module.
	const inputs = [
		entryModuleIds.clientEntry,
		...new Set(viewEntries.map((entry) => entry.id)),
		...(entryModuleIds.criticalCssEntry === undefined
			? []
			: [entryModuleIds.criticalCssEntry]),
	];
	function createStaticPublicAssetBuildPlugin() {
		return createStaticPublicAssetPlugin(
			{
				lookups: generation.inspection.staticPublicAssetLookups,
				sourceTexts: generation.inspection.clientSourceTexts,
				staticPublicAssets: generation.staticPublicAssets.assets,
			},
			{
				kind: "build",
				inspectedFiles: generation.inspection.inspectedClientFiles,
			},
		);
	}
	const plugin: Plugin = {
		name: "vorma:client-build",
		enforce: "post",
		applyToEnvironment: (environment) => environment.name === "client",
		configResolved: {
			order: "post",
			handler(config) {
				const checked = checkVormaPlugin({
					plugins: config.plugins,
					configFilePath: generation.sourceLayout.configFilePath,
				});
				if (Result.isFailure(checked)) {
					return this.error(checked.failure);
				}
			},
		},
		// Vite merges what the hooks return with the application's
		// configuration, so each hook returns only Vorma's own additions.
		config() {
			return {
				...createBuildViteOptions({
					base: generation.sourceLayout
						.staticPublicAssetsServeUrlPathPrefix,
				}),
				// Vite calls this function for each worker bundle.
				worker: {
					plugins: () => {
						return [
							...publicCss.workerPlugins,
							createStaticPublicAssetBuildPlugin(),
							...createClientOutputHashingPlugins({
								nativeOutputHashes,
							}),
						];
					},
				},
			};
		},
		configEnvironment(name, config) {
			if (name !== "client") {
				return;
			}
			if (Array.isArray(config.build?.rolldownOptions?.output)) {
				return this.error(
					"Vorma requires one output for its client environment",
				);
			}
			return createClientBuildViteOptions({
				outDir: generation.sourceLayout.publicDir,
				inputs,
			});
		},
		generateBundle: {
			order: "post",
			handler(_options, bundle) {
				const config = this.environment.config;
				if (
					config.build.watch ||
					config.build.lib ||
					config.build.ssr
				) {
					return this.error(
						"Vorma requires a non-watching browser client environment",
					);
				}
				const prepared = getBuildState();
				if (prepared.kind !== "awaiting-output") {
					return this.error(
						"Vorma requires one completed client output",
					);
				}
				const { criticalCssFileName } = prepared;
				const droppedFiles = new Set<string>();
				let criticalCss = "";
				if (criticalCssFileName !== undefined) {
					const stylesheet = bundle[criticalCssFileName];
					if (
						stylesheet === undefined ||
						stylesheet.type !== "asset"
					) {
						return this.error(
							"Vite did not preserve the compiled critical stylesheet",
						);
					}
					// Vite's CSS pipeline has already minified the stylesheet with
					// the application's build.cssMinify and build.cssTarget.
					criticalCss =
						typeof stylesheet.source === "string"
							? stylesheet.source
							: new TextDecoder().decode(stylesheet.source);
					const source =
						entryModuleIds.criticalCssEntry === undefined
							? null
							: this.getModuleInfo(
									entryModuleIds.criticalCssEntry,
								);
					const referenced =
						(source?.importers.length ?? 0) > 0 ||
						(source?.dynamicImporters.length ?? 0) > 0 ||
						Object.values(bundle).some((output) => {
							return (
								output.type === "chunk" &&
								(output.viteMetadata?.importedCss.has(
									stylesheet.fileName,
								) ||
									output.viteMetadata?.importedAssets.has(
										stylesheet.fileName,
									))
							);
						});
					if (!referenced) {
						// Rolldown keeps a deleted output in the captured bundle object,
						// so the deletion is recorded to leave the file out of the result.
						delete bundle[stylesheet.fileName];
						droppedFiles.add(stylesheet.fileName);
					}
				}
				const manifestFiles = new Set<string>();
				for (const [setting, defaultName] of [
					[config.build.manifest, ".vite/manifest.json"],
					[config.build.ssrManifest, ".vite/ssr-manifest.json"],
				] as const) {
					if (setting) {
						manifestFiles.add(
							typeof setting === "string" ? setting : defaultName,
						);
					}
				}
				buildState = {
					kind: "captured",
					bundle,
					nativeOutputHashes,
					preGenerateBundleChunkHashes,
					criticalCss,
					manifestFiles,
					droppedFiles,
				};
			},
		},
	};
	function forClient(plugin: Plugin): Plugin {
		return {
			...plugin,
			applyToEnvironment: (environment) => environment.name === "client",
		};
	}
	await build({
		configFile: generation.sourceLayout.viteConfigFilePath,
		mode: "production",
		plugins: [
			createViteConfigCheckPlugin({
				configFilePath: generation.sourceLayout.configFilePath,
				viteConfigFilePath: generation.sourceLayout.viteConfigFilePath,
			}),
			plugin,
			forClient({
				name: "vorma:critical-css",
				generateBundle: {
					order: "pre",
					handler(_options, bundle) {
						// Vite removes pure-CSS JavaScript chunks during generateBundle.
						// Record their native CSS association before that removal.
						const critical = findCriticalCssFileName({
							sourceId: entryModuleIds.criticalCssEntry,
							outputs: Object.values(bundle),
						});
						if (Result.isFailure(critical)) {
							return this.error(critical.failure);
						}
						if (getBuildState().kind !== "awaiting-css") {
							return this.error(
								"Vorma requires one critical stylesheet inspection per client build",
							);
						}
						buildState = {
							kind: "awaiting-output",
							criticalCssFileName: critical.success,
						};
					},
				},
			}),
			...publicCss.plugins.map(forClient),
			forClient(createStaticPublicAssetBuildPlugin()),
			...createClientOutputHashingPlugins({
				nativeOutputHashes,
				preGenerateBundleChunkHashes,
			}).map(forClient),
		],
	});
	return getBuildState();
}
