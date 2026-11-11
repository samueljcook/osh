import { readFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { SourceMap, type SourceMapPayload } from "node:module";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Result } from "effect";
import { RolldownMagicString } from "rolldown";
import { DevEnvironment, normalizePath, type Plugin } from "vite";
import type { StaticPublicAssetLookup } from "../inspection/inspect-projects.ts";
import { createStaticPublicAssetsReadGraph } from "./static-public-assets-reads.ts";
import type { StaticPublicAsset } from "./static-public-assets.ts";

/*
TypeScript resolves the asset binding and records literal lookups before source
transforms. The load hook replaces those lookups in the exact inspected text.
For other bundled modules, Babel resolves lexical bindings and this plugin
follows ESM exports and constant aliases. Only reads of the actual Vorma asset
binding fail; unrelated property names and shadowed bindings are ordinary code.
*/

/** The language in which the bundler parses each module type of JavaScript or TypeScript. */
const SCRIPT_MODULE_LANGUAGES: ReadonlyMap<
	string,
	"js" | "jsx" | "ts" | "tsx"
> = new Map([
	["js", "js"],
	["jsx", "jsx"],
	["ts", "ts"],
	["tsx", "tsx"],
]);

/** The id prefix of Vorma's own virtual modules. */
const VORMA_VIRTUAL_MODULE_PREFIX = "\0vorma:";

type StaticPublicAssetPluginInput = {
	readonly lookups: ReadonlyArray<StaticPublicAssetLookup>;
	readonly sourceTexts: ReadonlyMap<string, string>;
	readonly staticPublicAssets: ReadonlyArray<StaticPublicAsset>;
};

/** How Vite runs the plugin, and what each use needs. */
export type StaticPublicAssetPluginUse =
	| {
			readonly kind: "development";
			/** Requests inspection of changed development source; false means inspection failed. */
			readonly inspectChangedSourceFn: (
				filePath: string,
			) => Promise<boolean>;
	  }
	| {
			readonly kind: "build";
			/** Every client source file that the inspection read, as Vite's `normalizePath` spells it. */
			readonly inspectedFiles: ReadonlySet<string>;
	  };

/** What a production build records while it bundles. */
type BuildRecords = {
	/** The reference that the build emitted for each asset URL. */
	readonly referenceIds: Map<string, string>;
	/**
	 * The file names of the emitted assets. Each emitted asset is an empty
	 * stand-in for a file that is already in the public directory, so the
	 * bundle drops it before Rolldown writes it.
	 */
	readonly emittedFileNames: Set<string>;
	/** The IDs of the modules in which the plugin replaced lookups. */
	readonly replacedIds: Set<string>;
	readonly readGraph: ReturnType<typeof createStaticPublicAssetsReadGraph>;
	readonly describeReadById: Map<string, (offset: number) => string>;
};

type StaticPublicAssetPluginState =
	| (Extract<StaticPublicAssetPluginUse, { readonly kind: "development" }> & {
			readonly readGraph: ReturnType<
				typeof createStaticPublicAssetsReadGraph
			>;
	  })
	| (Extract<StaticPublicAssetPluginUse, { readonly kind: "build" }> &
			BuildRecords);

type StaticPublicAssetPluginApi = {
	readonly updateInput: (input: StaticPublicAssetPluginInput) => void;
};

export function createStaticPublicAssetPlugin(
	input: StaticPublicAssetPluginInput,
	use: StaticPublicAssetPluginUse,
): Plugin<StaticPublicAssetPluginApi> & {
	readonly api: StaticPublicAssetPluginApi;
} {
	const state: StaticPublicAssetPluginState =
		use.kind === "development"
			? {
					...use,
					readGraph: createStaticPublicAssetsReadGraph(),
				}
			: {
					...use,
					referenceIds: new Map(),
					emittedFileNames: new Set(),
					replacedIds: new Set(),
					readGraph: createStaticPublicAssetsReadGraph(),
					describeReadById: new Map(),
				};
	let projection = prepareStaticPublicAssetReplacements(input);
	function testInspectedDevelopmentFile(filePath: string): boolean {
		return (
			projection.sourceTexts.has(filePath) ||
			projection.sourceTexts.has(normalizePath(filePath))
		);
	}
	function invalidateUninspectedGraphModules(invalidation: {
		readonly changedFilePath: string;
		readonly evictedIds: ReadonlySet<string>;
		readonly moduleGraph: DevEnvironment["moduleGraph"];
	}): void {
		if (state.kind !== "development") {
			return;
		}
		const normalizedChanged = normalizePath(invalidation.changedFilePath);
		const seedModules = new Set([
			...(invalidation.moduleGraph.getModulesByFile(
				invalidation.changedFilePath,
			) ?? []),
			...(invalidation.moduleGraph.getModulesByFile(normalizedChanged) ??
				[]),
		]);
		for (const evictedId of invalidation.evictedIds) {
			const byId = invalidation.moduleGraph.getModuleById(evictedId);
			if (byId !== undefined) {
				seedModules.add(byId);
			}
			const evictedFilePath = stripModuleQuery(evictedId);
			for (const moduleNode of invalidation.moduleGraph.getModulesByFile(
				evictedFilePath,
			) ?? []) {
				seedModules.add(moduleNode);
			}
		}
		const visited = new Set(seedModules);
		const queue = [...seedModules];
		for (
			let current = queue.pop();
			current !== undefined;
			current = queue.pop()
		) {
			if (
				current.file !== null &&
				!current.id?.startsWith(VORMA_VIRTUAL_MODULE_PREFIX) &&
				!testInspectedDevelopmentFile(current.file)
			) {
				invalidation.moduleGraph.invalidateModule(current);
			}
			for (const importer of current.importers) {
				if (visited.has(importer)) {
					continue;
				}
				visited.add(importer);
				queue.push(importer);
				if (importer.id !== null) {
					state.readGraph.deleteModule(importer.id);
				}
			}
		}
	}
	return {
		name: "vorma:static-public-assets",
		api: {
			updateInput(next) {
				if (state.kind === "development") {
					const previousTexts = projection.sourceTexts;
					for (const filePath of new Set([
						...previousTexts.keys(),
						...next.sourceTexts.keys(),
					])) {
						if (
							previousTexts.get(filePath) !==
							next.sourceTexts.get(filePath)
						) {
							state.readGraph.deleteModule(filePath);
						}
					}
				}
				projection = prepareStaticPublicAssetReplacements(next);
			},
		},
		enforce: "post",
		configureServer(server) {
			if (state.kind !== "development") {
				return;
			}
			function handleWatchedFileChange(filePath: string) {
				if (state.kind !== "development") {
					return;
				}
				const evictedIds = state.readGraph.deleteModule(filePath);
				for (const environment of Object.values(server.environments)) {
					invalidateUninspectedGraphModules({
						changedFilePath: filePath,
						evictedIds,
						moduleGraph: environment.moduleGraph,
					});
				}
			}
			server.watcher.on("change", handleWatchedFileChange);
			server.watcher.on("unlink", handleWatchedFileChange);
		},
		hotUpdate(options) {
			if (state.kind !== "development") {
				return;
			}
			const evictedIds = new Set(
				state.readGraph.deleteModule(options.file),
			);
			for (const moduleNode of options.modules) {
				if (moduleNode.id !== null) {
					for (const id of state.readGraph.deleteModule(
						moduleNode.id,
					)) {
						evictedIds.add(id);
					}
				}
			}
			invalidateUninspectedGraphModules({
				changedFilePath: options.file,
				evictedIds,
				moduleGraph: this.environment.moduleGraph,
			});
		},
		transform: {
			// The check reads the code that the bundler bundles, after every
			// other plugin's transform.
			order: "post",
			async handler(code, id, options) {
				const queryIndex = id.indexOf("?");
				const filePath =
					queryIndex === -1 ? id : id.slice(0, queryIndex);
				const virtual = id.startsWith("\0") || !isAbsolute(filePath);
				const language =
					SCRIPT_MODULE_LANGUAGES.get(options?.moduleType ?? "") ??
					(state.kind === "development"
						? inferScriptModuleLanguage({ filePath, virtual })
						: undefined);
				if (language === undefined) {
					return null;
				}
				const devEnvironment =
					this.environment instanceof DevEnvironment
						? this.environment
						: undefined;
				function findOptimizedDep(candidatePath: string) {
					return devEnvironment?.depsOptimizer?.metadata.depInfoList.find(
						(candidate) => candidate.file === candidatePath,
					);
				}
				const owner = virtual
					? undefined
					: findOwningPackage(
							dirname(
								findOptimizedDep(filePath)?.src ?? filePath,
							),
						);
				if (owner?.name === "@vorma/client") {
					return null;
				}
				const resolveFn = this.resolve.bind(this);
				const visitedImports = new Set<string>([id]);
				async function resolveImportForGraph(input: {
					readonly importerId: string;
					readonly source: string;
				}) {
					const directResolved = await resolveFn(
						input.source,
						input.importerId,
					);
					let resolvedId = directResolved?.id;
					if (
						resolvedId === undefined &&
						devEnvironment !== undefined
					) {
						const { base } = devEnvironment.config;
						const moduleUrl =
							base !== "/" && input.source.startsWith(base)
								? `/${input.source.slice(base.length)}`
								: input.source;
						const moduleNode =
							await devEnvironment.moduleGraph.getModuleByUrl(
								moduleUrl,
							);
						resolvedId = moduleNode?.id ?? undefined;
					}
					if (resolvedId === undefined) {
						return undefined;
					}
					const resolvedQueryIndex = resolvedId.indexOf("?");
					const resolvedFilePath =
						resolvedQueryIndex === -1
							? resolvedId
							: resolvedId.slice(0, resolvedQueryIndex);
					const optimizedDep = findOptimizedDep(resolvedFilePath);
					const packageEntryPath =
						optimizedDep?.src ?? resolvedFilePath;
					const resolvedOwnerName = findOwningPackage(
						dirname(packageEntryPath),
					)?.name;
					let isClientEntry = false;
					if (resolvedOwnerName === "@vorma/client") {
						if (optimizedDep !== undefined) {
							isClientEntry = optimizedDep.id === "@vorma/client";
						} else if (input.source === "@vorma/client") {
							isClientEntry = true;
						} else if (devEnvironment !== undefined) {
							const expectedClientEntry = await resolveFn(
								"@vorma/client",
								input.importerId,
							);
							const expectedPath =
								expectedClientEntry === null
									? undefined
									: stripModuleQuery(expectedClientEntry.id);
							isClientEntry = expectedPath === resolvedFilePath;
						}
					}
					if (
						state.kind === "development" &&
						!isClientEntry &&
						!state.readGraph.testRecordedModule(resolvedId) &&
						!visitedImports.has(resolvedId)
					) {
						visitedImports.add(resolvedId);
						const resolvedVirtual =
							resolvedId.startsWith("\0") ||
							!isAbsolute(resolvedFilePath);
						const resolvedLanguage = resolvedVirtual
							? undefined
							: inferScriptModuleLanguage({
									filePath: resolvedFilePath,
									virtual: false,
								});
						if (
							resolvedLanguage !== undefined &&
							resolvedOwnerName !== "@vorma/client"
						) {
							const importedText = await readFile(
								resolvedFilePath,
								"utf8",
							).then(Result.succeed, Result.fail);
							if (Result.isSuccess(importedText)) {
								await state.readGraph.recordModule({
									id: resolvedId,
									code: removeByteOrderMark(
										importedText.success,
									),
									language: resolvedLanguage,
									resolveImportFn: (nestedSource) => {
										return resolveImportForGraph({
											importerId: resolvedId,
											source: nestedSource,
										});
									},
								});
							}
						}
					}
					return {
						id: resolvedId,
						isClientEntry,
					};
				}
				await state.readGraph.recordModule({
					id,
					code,
					language,
					resolveImportFn: (source) =>
						resolveImportForGraph({ importerId: id, source }),
				});
				if (
					id.startsWith(VORMA_VIRTUAL_MODULE_PREFIX) ||
					(state.kind === "build"
						? state.inspectedFiles.has(filePath)
						: projection.sourceTexts.has(filePath) ||
							projection.sourceTexts.has(normalizePath(filePath)))
				) {
					return null;
				}
				const map = Result.try(() => this.getCombinedSourcemap());
				const origin: UninspectedModuleOrigin = virtual
					? { kind: "virtual" }
					: id.includes("/node_modules/") ||
						  (owner !== undefined &&
								owner.dir !==
									findOwningPackage(
										this.environment.config.root,
									)?.dir)
						? { kind: "package", name: owner?.name }
						: { kind: "application" };
				function describeRead(offset: number): string {
					return describeUninspectedRead({
						location: describeUseLocation({
							code,
							offset,
							filePath,
							map: Result.isSuccess(map)
								? {
										file: map.success.file,
										version: map.success.version,
										sources: map.success.sources,
										sourcesContent:
											map.success.sourcesContent,
										names: map.success.names,
										mappings: map.success.mappings,
										sourceRoot: "",
									}
								: undefined,
						}),
						origin,
					});
				}
				if (state.kind === "development") {
					const read = state.readGraph.findRead(id);
					if (read !== undefined) {
						return this.error(describeRead(read.offset));
					}
					return null;
				}
				state.describeReadById.set(id, describeRead);
				return null;
			},
		},
		async load(id) {
			const queryIndex = id.indexOf("?");
			const filePath = queryIndex === -1 ? id : id.slice(0, queryIndex);
			let selected = projection;
			let current: string;
			while (true) {
				selected = projection;
				if (!selected.lookupsByFile.has(filePath)) {
					return null;
				}
				// The compiler's text and its lookup offsets exclude a byte order mark.
				current = removeByteOrderMark(await readFile(filePath, "utf8"));
				if (current === selected.sourceTexts.get(filePath)) {
					break;
				}
				if (
					state.kind === "development" &&
					(await state.inspectChangedSourceFn(filePath))
				) {
					continue;
				}
				return this.error(
					`${filePath} changed after the build read it; run the build again`,
				);
			}
			const { assets, lookupsByFile } = selected;
			const lookups = lookupsByFile.get(filePath);
			const code = new RolldownMagicString(current);
			for (const lookup of lookups!) {
				const asset = assets.get(lookup.key);
				if (asset === undefined) {
					return this.error(
						`Unknown static public asset: ${lookup.key}`,
					);
				}
				if (state.kind === "development") {
					code.overwrite(
						lookup.start,
						lookup.end,
						JSON.stringify(asset.url),
					);
					continue;
				}
				// Vite's asset plugin writes the URL of an emitted file and
				// records the file as an asset that the chunk imports.
				let referenceId = state.referenceIds.get(asset.url);
				if (referenceId === undefined) {
					referenceId = this.emitFile({
						type: "asset",
						fileName: asset.fileName,
						source: new Uint8Array(0),
					});
					state.referenceIds.set(asset.url, referenceId);
					state.emittedFileNames.add(asset.fileName);
				}
				code.overwrite(
					lookup.start,
					lookup.end,
					`import.meta.ROLLUP_FILE_URL_${referenceId}`,
				);
			}
			if (state.kind === "build") {
				state.replacedIds.add(id);
			}
			return {
				code: code.toString(),
				map: code
					.generateMap({
						source: id,
						includeContent: true,
						hires: true,
					})
					.toString(),
			};
		},
		generateBundle(_options, bundle) {
			if (state.kind !== "build") {
				return;
			}
			for (const fileName of state.emittedFileNames) {
				delete bundle[fileName];
			}
			const issues: Array<string> = [];
			for (const output of Object.values(bundle)) {
				if (output.type !== "chunk") {
					continue;
				}
				for (const id of Object.keys(output.modules)) {
					const queryIndex = id.indexOf("?");
					const filePath =
						queryIndex === -1 ? id : id.slice(0, queryIndex);
					// A module id with a query, such as a worker or a raw-text
					// import, names code that Vite generated, not the file's code.
					if (
						queryIndex === -1 &&
						projection.lookupsByFile.has(filePath) &&
						!state.replacedIds.has(id)
					) {
						issues.push(
							`${filePath} has staticPublicAssets lookups, but another plugin loaded the file, so the build could not replace them.`,
						);
					} else {
						const describeRead = state.describeReadById.get(id);
						const read =
							describeRead === undefined
								? undefined
								: state.readGraph.findRead(id);
						if (read !== undefined) {
							issues.push(describeRead!(read.offset));
						}
					}
				}
			}
			if (issues.length > 0) {
				return this.error(issues.join("\n"));
			}
		},
	};
}

/**
 * The origin of a module that the inspection did not read: a plugin that
 * generates a virtual module, a package other than the application, or the
 * application.
 */
type UninspectedModuleOrigin =
	| { readonly kind: "virtual" }
	| { readonly kind: "package"; readonly name: string | undefined }
	| { readonly kind: "application" };

/** Explains why a module's asset read could not be replaced. */
function describeUninspectedRead(input: {
	readonly location: string;
	readonly origin: UninspectedModuleOrigin;
}): string {
	const { location, origin } = input;

	switch (origin.kind) {
		case "virtual":
			return `${location} reads staticPublicAssets, but the module is a virtual module that a plugin generates, which no TypeScript project can contain, so the build cannot replace its lookups. Do not read staticPublicAssets in the code that the plugin generates.`;
		case "package":
			return `${location} reads staticPublicAssets, but the file belongs to the package ${origin.name ?? "that contains it"} and is outside the client TypeScript project, and built JavaScript cannot use staticPublicAssets lookups because the build replaces lookups only in the files of that project. Include the package's TypeScript source in the TypeScript project of client.tsConfigFile (for example, by importing the source instead of the built file) so that the build can replace its lookups, or do not read staticPublicAssets in the file.`;
		case "application":
			return `${location} reads staticPublicAssets, but the client TypeScript project does not contain the file, so the build cannot replace its lookups. Include the file in the TypeScript project of client.tsConfigFile so that the build can replace its lookups, or do not read staticPublicAssets in the file.`;
	}
}

/** A package directory and its name. */
type OwningPackage = {
	readonly dir: string;
	readonly name: string;
};

/**
 * The package that contains a directory: the nearest directory at or above it
 * with a `package.json` that has a name. A `package.json` without a name, as
 * a build output directory can include to set its module type, does not end the
 * search.
 */
function findOwningPackage(dir: string): OwningPackage | undefined {
	let current = dir;
	while (true) {
		const manifest = Result.try((): unknown => {
			return JSON.parse(
				readFileSync(join(current, "package.json"), "utf8"),
			);
		});
		if (
			Result.isSuccess(manifest) &&
			typeof manifest.success === "object" &&
			manifest.success !== null &&
			"name" in manifest.success &&
			typeof manifest.success.name === "string"
		) {
			return { dir: current, name: manifest.success.name };
		}
		const parent = dirname(current);
		if (parent === current) {
			return undefined;
		}
		current = parent;
	}
}

/**
 * Where a use is: its line and column in the module's file, when the combined
 * source map maps the use there, and otherwise its line and column in the
 * code that the bundler bundles for the module. Lines and columns count from 1,
 * and columns count UTF-16 code units, as the source map does.
 */
function describeUseLocation(input: {
	readonly code: string;
	readonly offset: number;
	readonly filePath: string;
	readonly map: SourceMapPayload | undefined;
}): string {
	const { code, offset, filePath, map } = input;
	// A virtual module's id starts with a NUL character, which a terminal does not show.
	const shownPath = filePath.replaceAll("\0", "\\0");
	const lines = code.slice(0, offset).split(/\r\n|[\n\r\u2028\u2029]/);
	const line = lines.length - 1;
	const column = lines[line]?.length ?? 0;
	const entry =
		map === undefined
			? undefined
			: Result.try(() => new SourceMap(map).findEntry(line, column));
	if (
		entry !== undefined &&
		Result.isSuccess(entry) &&
		"originalSource" in entry.success &&
		entry.success.generatedLine === line &&
		testSameFile({ source: entry.success.originalSource, filePath })
	) {
		const original = entry.success;
		return `${shownPath}:${original.originalLine + 1}:${original.originalColumn + column - original.generatedColumn + 1}`;
	}
	return `${shownPath} (line ${line + 1}, column ${column + 1} of the code that the bundler bundles for it)`;
}

/** Whether a source map's source names the module's file. */
function testSameFile(input: {
	readonly source: string;
	readonly filePath: string;
}): boolean {
	const sourcePath = Result.try(() => {
		return input.source.startsWith("file:")
			? fileURLToPath(input.source)
			: resolve(dirname(input.filePath), input.source);
	});
	return (
		Result.isSuccess(sourcePath) &&
		normalizePath(sourcePath.success) === normalizePath(input.filePath)
	);
}

function stripModuleQuery(id: string): string {
	const queryIndex = id.indexOf("?");
	return queryIndex === -1 ? id : id.slice(0, queryIndex);
}

function removeByteOrderMark(text: string): string {
	return text.startsWith("﻿") ? text.slice(1) : text;
}

function prepareStaticPublicAssetReplacements(
	input: StaticPublicAssetPluginInput,
) {
	const assets = new Map(
		input.staticPublicAssets.map((asset) => [asset.sourceKey, asset]),
	);
	const lookupsByFile = new Map<string, Array<StaticPublicAssetLookup>>();
	for (const lookup of input.lookups) {
		const lookups = lookupsByFile.get(lookup.filePath);
		if (lookups === undefined) {
			lookupsByFile.set(lookup.filePath, [lookup]);
		} else {
			lookups.push(lookup);
		}
	}
	return {
		assets,
		lookupsByFile,
		sourceTexts: input.sourceTexts,
	};
}

/**
 * Infers the parser language for a module in development when Vite's dev
 * server does not supply `options.moduleType`.
 */
function inferScriptModuleLanguage(input: {
	readonly filePath: string;
	readonly virtual: boolean;
}): "js" | "jsx" | "ts" | "tsx" | undefined {
	if (/\.(?:ts|mts|cts)$/i.test(input.filePath)) {
		return "ts";
	}
	if (/\.tsx$/i.test(input.filePath)) {
		return "tsx";
	}
	if (/\.jsx$/i.test(input.filePath)) {
		return "jsx";
	}
	if (/\.(?:js|mjs|cjs)$/i.test(input.filePath)) {
		return "js";
	}
	if (
		!input.virtual &&
		/\.(?:css|scss|sass|less|styl|stylus|pcss|postcss|json|wasm)$/i.test(
			input.filePath,
		)
	) {
		return undefined;
	}
	return "jsx";
}
