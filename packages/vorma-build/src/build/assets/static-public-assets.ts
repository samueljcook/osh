import { Effect, Order, Result } from "effect";
import * as FileSystem from "effect/FileSystem";
import * as Path from "effect/Path";
import type { ApplicationSourceLayout } from "../config/source-layout.ts";
import {
	encodeContentHash,
	hashFile,
	toContentHash,
} from "../output/content-hash.ts";
import { ARTIFACT_PROTOCOL } from "../output/dist-dir.ts";
import { toPosixRelativePath } from "../output/posix-relative-path.ts";
import { toPublishedFileUrl } from "../output/published-file-url.ts";

/** Names the temporary directory in the public directory through which the assets are copied. */
const ASSET_COPY_DIR_PREFIX = "_vorma-write-";

export type StaticPublicAssetIssue = {
	readonly _tag: "StaticPublicAssetIssue";
	readonly sourcePath: string;
	readonly msg: string;
	readonly cause?: unknown;
};
export type StaticPublicAsset = {
	readonly sourceKey: string;
	readonly sourcePath: string;
	/** Relative to the published public directory, with decoded path segments. */
	readonly fileName: string;
	readonly url: string;
	readonly hash: string;
};
export type StaticPublicAssetProjection = {
	readonly assets: ReadonlyArray<StaticPublicAsset>;
	readonly staticPublicAssetUrls: Readonly<Record<string, string>>;
	readonly uniqueStaticPublicAssetUrls: ReadonlyArray<string>;
};

/** The names, in lower case, that the public directory never publishes, at any depth and in any letter case. */
const IGNORED_STATIC_PUBLIC_ASSET_NAMES: ReadonlySet<string> = new Set([
	".ds_store",
	".env",
]);

/** The prefix, in lower case, of the names that the public directory never publishes, at any depth and in any letter case. */
const IGNORED_STATIC_PUBLIC_ASSET_NAME_PREFIX = ".env.";

/**
 * The characters that a published static public asset's name replaces with
 * `_`: those that destination file systems forbid, and those that a published
 * file name cannot hold (`toPublishedFileUrl`).
 */
const UNPUBLISHABLE_FILE_NAME_CHARACTERS =
	// oxlint-disable-next-line no-control-regex -- Published file names exclude control characters.
	/[<>:"/\\|?#*[\]%^\u0000-\u001f\u007f-\u009f]/g;

/** The longest file name, in UTF-8 bytes, that common file systems accept. */
const MAX_FILE_NAME_BYTES = 255;

/**
 * Whether the public directory skips a file of this name: env files hold
 * secrets by convention, and everything in the public output is public. The
 * comparison ignores letter case, because a file system that ignores letter
 * case reads `.ENV` as `.env`.
 */
export function testIgnoredStaticPublicAssetName(name: string): boolean {
	const lowerCaseName = name.toLowerCase();
	return (
		IGNORED_STATIC_PUBLIC_ASSET_NAMES.has(lowerCaseName) ||
		lowerCaseName.startsWith(IGNORED_STATIC_PUBLIC_ASSET_NAME_PREFIX)
	);
}

/**
 * Hashes and publishes each static public asset's exact bytes, retaining only
 * metadata. Entries named `.DS_Store` or `.env`, or starting with `.env.`, in
 * any letter case, are skipped at every depth. Each asset is copied into a
 * temporary directory in the public directory, hashed there, and renamed to
 * its published name, so the published bytes are the hashed bytes, no asset
 * is read whole into memory, and no reader of the public directory sees a
 * partial file. A published file that already has an asset's name is not
 * written again: its name carries the hash of its bytes, and the public
 * directory receives only complete files.
 */
export function collectStaticPublicAssets(
	config: ApplicationSourceLayout,
): Effect.Effect<
	StaticPublicAssetProjection,
	StaticPublicAssetIssue,
	FileSystem.FileSystem | Path.Path
> {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const path = yield* Path.Path;
		const root = config.staticPublicAssetsSourceDir;
		const pending = [{ dir: root, ancestors: new Set([root]) }];
		const files: Array<{ sourcePath: string; canonicalPath: string }> = [];
		while (pending.length > 0) {
			const dirs = pending.splice(0);
			const inspections = yield* Effect.forEach(
				dirs,
				({ dir, ancestors }) => {
					return Effect.gen(function* () {
						const names = yield* mapToStaticPublicAssetIssue({
							sourcePath: dir,
							msg: "Cannot list static public assets",
							effect: fileSystem.readDirectory(dir),
						});
						const entries = yield* Effect.forEach(
							names.filter(
								(name) =>
									!testIgnoredStaticPublicAssetName(name),
							),
							(name) => {
								const sourcePath = path.join(dir, name);
								return mapToStaticPublicAssetIssue({
									sourcePath,
									msg: "Cannot inspect static public asset",
									effect: Effect.gen(function* () {
										const canonicalPath =
											yield* fileSystem.realPath(
												sourcePath,
											);
										const stat =
											yield* fileSystem.stat(
												canonicalPath,
											);
										return {
											sourcePath,
											canonicalPath,
											type: stat.type,
										};
									}),
								});
							},
							{ concurrency: "unbounded" },
						);
						return { entries, ancestors };
					});
				},
				{ concurrency: "unbounded" },
			);
			for (const { entries, ancestors } of inspections) {
				for (const entry of entries) {
					if (entry.type === "File") {
						files.push(entry);
					} else if (entry.type === "Directory") {
						if (ancestors.has(entry.canonicalPath)) {
							return yield* Effect.fail({
								_tag: "StaticPublicAssetIssue",
								sourcePath: entry.sourcePath,
								msg: "Static public asset directories must not contain symbolic-link cycles",
							} satisfies StaticPublicAssetIssue);
						}
						pending.push({
							dir: entry.sourcePath,
							ancestors: new Set([
								...ancestors,
								entry.canonicalPath,
							]),
						});
					} else {
						return yield* Effect.fail({
							_tag: "StaticPublicAssetIssue",
							sourcePath: entry.sourcePath,
							msg: `Static public assets must be regular files or directories; found ${entry.type}`,
						} satisfies StaticPublicAssetIssue);
					}
				}
			}
		}
		files.sort(Order.mapInput(Order.String, (file) => file.sourcePath));
		const copyDir = yield* mapToStaticPublicAssetIssue({
			sourcePath: config.publicDir,
			msg: "Cannot create the temporary directory for static public assets",
			effect: fileSystem.makeTempDirectoryScoped({
				directory: config.publicDir,
				prefix: ASSET_COPY_DIR_PREFIX,
			}),
		});
		const assets = yield* Effect.forEach(
			files,
			({ sourcePath, canonicalPath }, index) => {
				function mapPublicationIssue<A, E, R>(
					effect: Effect.Effect<A, E, R>,
				) {
					return mapToStaticPublicAssetIssue({
						sourcePath,
						msg: "Cannot publish static public asset",
						effect,
					});
				}
				return Effect.gen(function* () {
					const sourceKey = toPosixRelativePath({
						path,
						from: root,
						to: sourcePath,
					});
					const copyPath = path.join(copyDir, String(index));
					yield* mapPublicationIssue(
						fileSystem.copyFile(canonicalPath, copyPath),
					);
					const hash = yield* mapPublicationIssue(hashFile(copyPath));
					const pathBytes = new TextEncoder().encode(sourceKey);
					const identity = new Uint8Array(pathBytes.length + 1 + 32);
					identity.set(pathBytes);
					identity.set(hash, pathBytes.length + 1);
					const suffix = toContentHash(identity).slice(0, 12);
					const basename = path.basename(sourceKey);
					const dot = basename.lastIndexOf(".");
					const extension = dot < 0 ? "" : basename.slice(dot);
					const stem = sourceKey
						.slice(0, sourceKey.length - extension.length)
						.replaceAll("/", "_");
					// The logical key and its hash retain the source spelling;
					// the published name has `_` in place of each character that
					// it cannot hold.
					const fileName =
						`${ARTIFACT_PROTOCOL.publicOutputFileNamePrefix}${stem}_${suffix}${extension}`
							.replaceAll(UNPUBLISHABLE_FILE_NAME_CHARACTERS, "_")
							.replace(/[. ]+$/, (tail) =>
								"_".repeat(tail.length),
							);
					const fileNameBytes = new TextEncoder().encode(
						fileName,
					).length;
					if (fileNameBytes > MAX_FILE_NAME_BYTES) {
						return yield* Effect.fail({
							_tag: "StaticPublicAssetIssue",
							sourcePath,
							msg: `The static public asset ${JSON.stringify(sourceKey)} would be published under a file name of ${fileNameBytes} bytes, but file systems accept at most ${MAX_FILE_NAME_BYTES} bytes. Shorten its path in core.staticPublicAssets.srcDir.`,
						} satisfies StaticPublicAssetIssue);
					}
					const url = toPublishedFileUrl({
						staticPublicAssetsServeUrlPathPrefix:
							config.staticPublicAssetsServeUrlPathPrefix,
						fileName,
					});
					if (Result.isFailure(url)) {
						return yield* Effect.fail({
							_tag: "StaticPublicAssetIssue",
							sourcePath,
							msg: `The static public asset ${JSON.stringify(sourceKey)} cannot be published: ${url.failure}`,
						} satisfies StaticPublicAssetIssue);
					}
					const publishedPath = path.join(config.publicDir, fileName);
					yield* mapPublicationIssue(
						Effect.gen(function* () {
							if (yield* fileSystem.exists(publishedPath)) {
								yield* fileSystem.remove(copyPath);
							} else {
								yield* fileSystem.rename(
									copyPath,
									publishedPath,
								);
							}
						}),
					);
					return {
						sourceKey,
						sourcePath,
						hash: encodeContentHash(hash),
						url: url.success,
						fileName,
					};
				});
			},
			{ concurrency: 16 },
		);
		const staticPublicAssetUrls = Object.fromEntries(
			assets.map((asset) => [asset.sourceKey, asset.url]),
		);
		return {
			assets,
			staticPublicAssetUrls,
			uniqueStaticPublicAssetUrls: [
				...new Set(assets.map((asset) => asset.url)),
			],
		};
	}).pipe(Effect.scoped);
}

/** An asset key union type, one key per line, at the given tab depth. */
export function renderAssetKeyDeclaration(input: {
	readonly prefix: string;
	readonly assets: ReadonlyArray<StaticPublicAsset>;
	readonly depth: number;
}): string {
	const [only, ...more] = input.assets;
	if (only === undefined) {
		return `${input.prefix} never;`;
	}
	if (more.length === 0) {
		return `${input.prefix} ${JSON.stringify(only.sourceKey)};`;
	}
	return (
		input.prefix +
		"\n" +
		input.assets
			.map(
				(asset) =>
					`${"\t".repeat(input.depth + 1)}| ${JSON.stringify(asset.sourceKey)}`,
			)
			.join("\n") +
		";"
	);
}

function mapToStaticPublicAssetIssue<A, E, R>(input: {
	readonly sourcePath: string;
	readonly msg: string;
	readonly effect: Effect.Effect<A, E, R>;
}): Effect.Effect<A, StaticPublicAssetIssue, R> {
	return input.effect.pipe(
		Effect.mapError((cause): StaticPublicAssetIssue => {
			return {
				_tag: "StaticPublicAssetIssue",
				sourcePath: input.sourcePath,
				msg: input.msg,
				cause,
			};
		}),
	);
}
