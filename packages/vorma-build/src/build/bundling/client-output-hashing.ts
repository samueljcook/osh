import { readFileSync, statSync } from "node:fs";
import {
	basename,
	dirname,
	extname,
	isAbsolute,
	relative,
	resolve,
} from "node:path";
import { blake3 } from "@noble/hashes/blake3.js";
import { utf8ToBytes } from "@noble/hashes/utils.js";
import { Result } from "effect";
import { normalizePath, type Plugin } from "vite";
import { describeCause } from "../issues.ts";
import { encodeContentHash, toContentHash } from "../output/content-hash.ts";
import { ARTIFACT_PROTOCOL } from "../output/dist-dir.ts";

// Vite's `vite:css-post` appends `/*$vite$:1*/` before emitting CSS assets so
// that Rolldown computes an output hash, and then strips the marker in
// `generateBundle`. Stripping the marker before hashing embeds the final CSS
// bytes' BLAKE3 hash in the emitted file name.
const VITE_CSS_HASH_MARKER = /\/\*\$vite\$:\d+\*\//;

const PRE_OUTPUT_HASHING_PLUGIN_NAME = "vorma:client-output-hashing-pre";

// Both `vite:css-post` and `vite:build-import-analysis` implement
// `augmentChunkHash` for their post-render chunk rewrites (pure-CSS chunk
// pruning and `__VITE_PRELOAD__` dependency lists) and apply those rewrites in
// `generateBundle`. Advancing an untainted chunk's expected hash across only
// those two built-in hooks preserves Vite's own hash-covered rewrites while
// rejecting user `generateBundle` chunk mutations.
const VITE_POST_HASH_CHUNK_PLUGINS: ReadonlySet<string> = new Set([
	"vite:css-post",
	"vite:build-import-analysis",
]);

const wrappedVitePostHashPlugins = new WeakSet<Plugin>();

/**
 * Creates the pre-`generateBundle` and post-`generateBundle` client output
 * hashing plugins for the client bundle or for a worker bundle.
 *
 * The pre-`generateBundle` plugin records each chunk's content hash before
 * user `generateBundle` hooks run so that the build can reject post-hash chunk
 * mutations. The post-`generateBundle` plugin names every file of the bundle
 * after the application's output options run and records only unmutated chunk
 * hashes in `nativeOutputHashes` for worker chunk re-emission.
 *
 * A source map contains, for each module of its chunk, the module's path
 * relative to the map and the module's original text, and either can change
 * while the chunk's code does not, as the rename of a module or an edit of a
 * comment does; the chunk names its map. When the build writes source maps,
 * the chunk's hash therefore also covers each module's path relative to the
 * map's directory (with its query) and its original text so that the chunk
 * and its map, which carries the chunk's hash, get new names whenever the map
 * changes. The paths are relative, so a build of the same sources in another
 * directory names its files the same way.
 */
export function createClientOutputHashingPlugins(input: {
	readonly nativeOutputHashes: Map<string, string>;
	readonly preGenerateBundleChunkHashes?: Map<string, string>;
}): ReadonlyArray<Plugin> {
	const preGenerateBundleChunkHashes =
		input.preGenerateBundleChunkHashes ?? new Map<string, string>();
	const nativeAssetHashLength = 20;
	const prePlugin: Plugin = {
		name: PRE_OUTPUT_HASHING_PLUGIN_NAME,
		enforce: "pre",
		configResolved(config) {
			const plugins = config.plugins as Array<Plugin>;
			const preIndex = plugins.findIndex(
				(candidate) =>
					candidate.name === PRE_OUTPUT_HASHING_PLUGIN_NAME,
			);
			if (preIndex > 0) {
				const [found] = plugins.splice(preIndex, 1);
				if (found !== undefined) {
					plugins.unshift(found);
				}
			}
			for (const candidate of plugins) {
				if (VITE_POST_HASH_CHUNK_PLUGINS.has(candidate.name)) {
					wrapVitePostHashGenerateBundlePlugin({
						plugin: candidate,
						preGenerateBundleChunkHashes,
					});
				}
			}
		},
		generateBundle: {
			order: "pre",
			handler(_options, bundle) {
				preGenerateBundleChunkHashes.clear();
				for (const output of Object.values(bundle)) {
					if (output.type === "chunk") {
						preGenerateBundleChunkHashes.set(
							output.fileName,
							toContentHash(output.code),
						);
					}
				}
			},
		},
	};
	const postPlugin: Plugin = {
		name: "vorma:client-output-hashing",
		enforce: "post",
		outputOptions: {
			order: "post",
			handler(options) {
				return {
					...options,
					...ARTIFACT_PROTOCOL.clientOutputFileNames,
					entryFileNames: (chunk) =>
						`${ARTIFACT_PROTOCOL.publicOutputFileNamePrefix}${toPortableOutputPathStem(chunk.name)}-[hash].js`,
					chunkFileNames: (chunk) =>
						`${ARTIFACT_PROTOCOL.publicOutputFileNamePrefix}${toPortableOutputPathStem(chunk.name)}-[hash].js`,
					sourcemapFileNames: (chunk) =>
						`${ARTIFACT_PROTOCOL.publicOutputFileNamePrefix}${toPortableOutputPathStem(chunk.name)}-[chunkhash].js.map`,
					assetFileNames(asset) {
						const name = basename(asset.names[0] ?? "asset");
						const extension = extname(name);
						const stem = toPortableOutputSegment(
							name.slice(0, name.length - extension.length),
						);
						const suffix = toPortableOutputChars(extension);
						const hashSource =
							suffix.endsWith(".css") &&
							typeof asset.source === "string"
								? asset.source.replace(VITE_CSS_HASH_MARKER, "")
								: asset.source;
						const prefix = `${ARTIFACT_PROTOCOL.publicOutputFileNamePrefix}${stem}-${toContentHash(hashSource)}-`;
						// Native hashing also covers references that Vite resolves after this hook.
						return `${prefix}[hash:${nativeAssetHashLength}]${suffix}`;
					},
				};
			},
		},
		generateBundle: {
			order: "post",
			handler(_options, bundle) {
				for (const output of Object.values(bundle)) {
					if (
						output.type !== "chunk" ||
						output.preliminaryFileName === output.fileName
					) {
						continue;
					}
					const codeHash = toContentHash(output.code);
					if (
						preGenerateBundleChunkHashes.get(output.fileName) !==
						codeHash
					) {
						continue;
					}
					input.nativeOutputHashes.set(output.fileName, codeHash);
					if (output.sourcemapFileName !== null) {
						const map = bundle[output.sourcemapFileName];
						if (map?.type === "asset") {
							input.nativeOutputHashes.set(
								map.fileName,
								toContentHash(map.source),
							);
						}
					}
				}
			},
		},
		augmentChunkHash(chunk) {
			const { config } = this.environment;
			if (!config.build.sourcemap) {
				return;
			}
			const mapDir = resolve(
				config.root,
				config.build.outDir,
				dirname(chunk.fileName),
			);
			const hasher = blake3.create();
			for (const id of chunk.moduleIds) {
				const queryIndex = id.indexOf("?");
				const filePath =
					queryIndex === -1 ? id : id.slice(0, queryIndex);
				// A virtual module's id names no file of the machine, and the map
				// names the module by its id.
				if (id.startsWith("\0") || !isAbsolute(filePath)) {
					hasher.update(utf8ToBytes(`${id}\0`));
					continue;
				}
				const query = queryIndex === -1 ? "" : id.slice(queryIndex);
				hasher.update(
					utf8ToBytes(
						`${normalizePath(relative(mapDir, filePath))}${query}\0`,
					),
				);
				// A module id that names no regular file has no original text
				// apart from the code in the chunk.
				const text = Result.try(() => {
					return statSync(filePath, {
						throwIfNoEntry: false,
					})?.isFile() === true
						? readFileSync(filePath)
						: undefined;
				});
				if (Result.isFailure(text)) {
					return this.error(
						`Vorma could not read ${filePath} to name the chunk and the source map that contain it: ${describeCause(text.failure)}`,
					);
				}
				if (text.success !== undefined) {
					hasher.update(blake3(text.success));
				}
			}
			return encodeContentHash(hasher.digest());
		},
	};
	return [prePlugin, postPlugin];
}

/**
 * Advances each untainted chunk's expected BLAKE3 content hash across one of
 * Vite's own hash-covered `generateBundle` hooks.
 */
function wrapVitePostHashGenerateBundlePlugin(input: {
	readonly plugin: Plugin;
	readonly preGenerateBundleChunkHashes: Map<string, string>;
}): void {
	const { plugin, preGenerateBundleChunkHashes } = input;
	if (
		wrappedVitePostHashPlugins.has(plugin) ||
		plugin.generateBundle === undefined
	) {
		return;
	}
	wrappedVitePostHashPlugins.add(plugin);
	const existing = plugin.generateBundle;
	const handler =
		typeof existing === "function" ? existing : existing.handler;
	async function runWrappedHandler(
		this: ThisParameterType<typeof handler>,
		...args: Parameters<typeof handler>
	): Promise<void> {
		const [, bundle] = args;
		const untaintedChunkFiles = new Set<string>();
		for (const output of Object.values(bundle)) {
			if (
				output.type === "chunk" &&
				preGenerateBundleChunkHashes.get(output.fileName) ===
					toContentHash(output.code)
			) {
				untaintedChunkFiles.add(output.fileName);
			}
		}
		await handler.apply(this, args);
		for (const fileName of untaintedChunkFiles) {
			const output = bundle[fileName];
			if (output?.type === "chunk") {
				preGenerateBundleChunkHashes.set(
					fileName,
					toContentHash(output.code),
				);
			}
		}
	}
	plugin.generateBundle =
		typeof existing === "function"
			? runWrappedHandler
			: { ...existing, handler: runWrappedHandler };
}

/** `toPortableOutputChars` replaces characters outside portable URL path syntax with underscores. */
function toPortableOutputChars(value: string): string {
	return value.replace(/[^a-zA-Z0-9._@-]/g, "_");
}

/** `toPortableOutputSegment` converts one segment into a non-empty relative path segment that is neither `.` nor `..`. */
function toPortableOutputSegment(segment: string): string {
	const portable = toPortableOutputChars(segment);
	return portable === "" || portable === "." || portable === ".."
		? "_"
		: portable;
}

/** `toPortableOutputPathStem` preserves `/` directory separators in chunk names and sanitizes each path segment. */
function toPortableOutputPathStem(name: string): string {
	return name.split("/").map(toPortableOutputSegment).join("/");
}
