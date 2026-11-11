import { pid } from "node:process";
import { Effect, FileSystem, Option, Order, Schema, type Scope } from "effect";
import * as Path from "effect/Path";
import type { PlatformError } from "effect/PlatformError";
import {
	checkPathContained,
	resolveCanonicalPath,
} from "primitives-ts/fs/path-containment/node";
import {
	readProcessLockOwnerRecord,
	tryAcquireProcessLock,
	type ProcessLockError,
} from "primitives-ts/processes/local-locks/node";
import { describeCause } from "../issues.ts";
import { ARTIFACT_PROTOCOL, getBuildDirPaths } from "./dist-dir.ts";

/**
 * The owner record that a build or development session writes in its lock.
 * It is diagnostic data for the message that a contender prints, and the
 * build never decides from it whether the lock is held.
 */
const Owner = Schema.fromJsonString(
	Schema.Struct({
		pid: Schema.Int.check(Schema.isGreaterThan(0)),
		kind: Schema.Literals(["build", "dev"]),
		dir: Schema.String,
	}),
);

/** The owner that an acquisition records. */
type LockOwner = typeof Owner.Type;

export type OutputOwnershipIssue = {
	readonly _tag: "OutputOwnershipIssue";
	readonly dir: string;
	readonly msg: string;
	readonly cause?: unknown;
};

/** The Vorma owner record of a lock directory; undefined when the lock holds none that Vorma wrote. */
function readLockOwner(lockPath: string): Effect.Effect<LockOwner | undefined> {
	return Effect.promise(() => {
		return readProcessLockOwnerRecord({ dir: lockPath });
	}).pipe(
		Effect.map((record) => {
			return record.kind === "ok" && record.value !== undefined
				? Option.getOrUndefined(
						Schema.decodeUnknownOption(Owner)(record.value),
					)
				: undefined;
		}),
	);
}

/** Resolves aliases even when the output directory has not been created. */
export function canonicalizeOutputPath(
	filePath: string,
): Effect.Effect<string, OutputOwnershipIssue> {
	return Effect.promise(() => resolveCanonicalPath({ filePath })).pipe(
		Effect.flatMap((resolved) => {
			if (resolved.kind === "ok") {
				return Effect.succeed(resolved.value);
			}
			const cause =
				resolved.error.kind === "io"
					? resolved.error.cause
					: new Error(resolved.error.msg);
			return Effect.fail({
				_tag: "OutputOwnershipIssue" as const,
				dir: filePath,
				msg: `Cannot resolve output path ${filePath}: ${describeCause(cause)}`,
				cause,
			});
		}),
	);
}

/**
 * Checks canonical containment before removing or replacing generated output:
 * the path must be inside the root directory, and no entry between them may
 * be a symbolic link.
 */
export function checkOutputPath(input: {
	readonly filePath: string;
	readonly rootDir: string;
}): Effect.Effect<void, OutputOwnershipIssue> {
	return Effect.promise(() => {
		return checkPathContained({
			dir: input.rootDir,
			filePath: input.filePath,
		});
	}).pipe(
		Effect.flatMap((checked) => {
			if (checked.kind === "ok") {
				return Effect.void;
			}
			switch (checked.error.kind) {
				case "outside-dir":
				case "symbolic-link-below-dir":
					return Effect.fail({
						_tag: "OutputOwnershipIssue" as const,
						dir: input.filePath,
						msg: `Generated output path leaves its directory: ${input.filePath}`,
					});
				case "dangling-symbolic-link":
				case "io": {
					const cause =
						checked.error.kind === "io"
							? checked.error.cause
							: new Error(checked.error.msg);
					return Effect.fail({
						_tag: "OutputOwnershipIssue" as const,
						dir: input.filePath,
						msg: `Cannot resolve output path ${checked.error.kind === "io" ? checked.error.path : checked.error.linkPath}: ${describeCause(cause)}`,
						cause,
					});
				}
			}
		}),
	);
}

/** The entries that Vorma writes in a build directory. */
const VORMA_BUILD_DIR_ENTRY_NAMES: ReadonlySet<string> = new Set([
	ARTIFACT_PROTOCOL.buildMetadataFileName,
	ARTIFACT_PROTOCOL.publicDirName,
	ARTIFACT_PROTOCOL.lockDirName,
	ARTIFACT_PROTOCOL.stagingDirName,
]);

/** The entries that a build directory without build metadata may hold: those that a running build or session holds while it has published nothing. */
const UNPUBLISHED_BUILD_DIR_ENTRY_NAMES: ReadonlySet<string> = new Set([
	ARTIFACT_PROTOCOL.lockDirName,
	ARTIFACT_PROTOCOL.stagingDirName,
]);

/** The entries that a build directory may hold beside a lock with a readable Vorma owner record: those that a session that ended without its cleanup leaves. */
const LOCKED_BUILD_DIR_ENTRY_NAMES: ReadonlySet<string> = new Set([
	ARTIFACT_PROTOCOL.publicDirName,
	ARTIFACT_PROTOCOL.lockDirName,
	ARTIFACT_PROTOCOL.stagingDirName,
]);

/** The entries of a directory, as a message names them: at most five, and the number of the others. */
function describeDirEntries(entries: ReadonlyArray<string>): string {
	const sorted = [...entries].sort(Order.String);
	const named = sorted.slice(0, 5).map((entry) => JSON.stringify(entry));
	const others = sorted.length - named.length;
	return [
		...named,
		...(others === 0
			? []
			: [`${others} other ${others === 1 ? "entry" : "entries"}`]),
	].join(", ");
}

/**
 * Refuses a build directory that holds something other than Vorma's output,
 * which guards against a `core.distDir` in which `prod` or `dev` is already a
 * directory of the application. A directory belongs to Vorma when it does not
 * exist, when it is empty, when it holds Vorma build metadata, when it holds
 * nothing other than the lock and the staging directory, or when it holds
 * `public`, with or without the staging directory, beside a lock with a
 * readable Vorma owner record, whether its owner is alive or not. A
 * development session that ends without its cleanup before its first
 * metadata leaves the last of these: an owner removes its record when it
 * releases the lock, so the record remains only after an owner that ended
 * without releasing the lock, or while an owner runs.
 */
function checkBuildDirContents(input: {
	readonly fileSystem: FileSystem.FileSystem;
	readonly path: Path.Path;
	readonly dir: string;
}): Effect.Effect<void, OutputOwnershipIssue> {
	const { fileSystem, path } = input;
	return Effect.gen(function* () {
		const entries = yield* fileSystem.readDirectory(input.dir).pipe(
			Effect.catchReason("PlatformError", "NotFound", () => {
				return Effect.succeed<ReadonlyArray<string>>([]);
			}),
		);
		if (
			entries.includes(ARTIFACT_PROTOCOL.buildMetadataFileName) ||
			entries.every((entry) =>
				UNPUBLISHED_BUILD_DIR_ENTRY_NAMES.has(entry),
			)
		) {
			return;
		}
		if (
			entries.includes(ARTIFACT_PROTOCOL.lockDirName) &&
			entries.every((entry) => LOCKED_BUILD_DIR_ENTRY_NAMES.has(entry)) &&
			(yield* readLockOwner(
				path.join(input.dir, ARTIFACT_PROTOCOL.lockDirName),
			)) !== undefined
		) {
			return;
		}
		return yield* Effect.fail({
			_tag: "OutputOwnershipIssue" as const,
			dir: input.dir,
			msg: `Vorma refuses to use ${input.dir} as its build directory: it holds ${describeDirEntries(entries)}, and no Vorma build: no ${ARTIFACT_PROTOCOL.buildMetadataFileName}, and no ${ARTIFACT_PROTOCOL.lockDirName} with a Vorma owner record beside ${ARTIFACT_PROTOCOL.publicDirName}. Check core.distDir.`,
		});
	}).pipe(
		Effect.mapError((cause): OutputOwnershipIssue => {
			return cause._tag === "OutputOwnershipIssue"
				? cause
				: {
						_tag: "OutputOwnershipIssue",
						dir: input.dir,
						msg: `Cannot read the build directory ${input.dir}: ${cause.message}`,
						cause,
					};
		}),
	);
}

/**
 * Removes the `public` directory of a build directory that holds no build
 * metadata. A Vorma process wrote that directory: when the owner took the
 * build directory, it held no `public`, or a `public` beside a Vorma lock
 * record. A development session writes `public` before its first metadata,
 * and without this removal, a session that ends with its cleanup before its
 * first metadata would leave `public` without a lock, which the next owner
 * refuses.
 */
function removeUnpublishedPublicDir(input: {
	readonly fileSystem: FileSystem.FileSystem;
	readonly path: Path.Path;
	readonly dir: string;
}): Effect.Effect<void, PlatformError> {
	const { fileSystem, path, dir } = input;
	return Effect.gen(function* () {
		const published = yield* fileSystem.exists(
			path.join(dir, ARTIFACT_PROTOCOL.buildMetadataFileName),
		);
		if (!published) {
			yield* fileSystem.remove(
				path.join(dir, ARTIFACT_PROTOCOL.publicDirName),
				{ recursive: true, force: true },
			);
		}
	});
}

/** The build metadata that `writeBuildMetadata` wrote in a staging directory. */
export type WrittenBuildMetadata = {
	readonly _tag: "WrittenBuildMetadata";
	readonly filePath: string;
	/** The files of the staged `public` that the metadata lists, by their POSIX paths in `public`. */
	readonly staticPublicAssetFileNames: ReadonlyArray<string>;
};

/** A build directory that the application's server loaded in a check run. */
export type CheckedBuildDir = {
	readonly _tag: "CheckedBuildDir";
	readonly buildDir: string;
};

/**
 * A production build's output, staged inside the build directory. Publication
 * first compares the staged `public` with the files that the written metadata
 * lists and fails on any difference. It then moves each staged file of
 * `public` to the same path in the build directory's `public`, replaces the
 * build metadata in one rename, and then removes the published files and
 * directories that the staged build does not have. The name of every file
 * that Vorma names in `public`, a static public asset, a chunk, a Vite asset,
 * or a source map, carries a hash that covers the file's content, so a move
 * never changes the bytes under a name that the previous build lists, and
 * the build directory holds one complete build: the previous build until the
 * metadata is replaced, and the staged build after that. Plugin assets must
 * also have content-hashed names. Metadata publication verifies that the
 * written files match the bundle's content identities. A build that fails
 * before publication leaves the previous build intact. Closing the scope
 * removes the staging directory.
 */
export type StagedBuildDir = {
	/** The staging directory, where the build writes. */
	readonly stagingDir: string;
	/**
	 * Publishes the staged build after `writeBuildMetadata` wrote its metadata
	 * and the application server loaded its directory during the check run.
	 */
	readonly publish: (input: {
		readonly metadata: WrittenBuildMetadata;
		readonly check: CheckedBuildDir;
	}) => Effect.Effect<void, OutputOwnershipIssue>;
};

/** The entries of a directory, or none when the directory does not exist. */
function readDirIfPresent(input: {
	readonly fileSystem: FileSystem.FileSystem;
	readonly dir: string;
}) {
	return input.fileSystem.readDirectory(input.dir).pipe(
		Effect.catchReason("PlatformError", "NotFound", () => {
			return Effect.succeed<ReadonlyArray<string>>([]);
		}),
	);
}

/** One entry of a directory tree: its POSIX path below the tree's root and its type. */
type TreeEntry = {
	readonly name: string;
	readonly type: FileSystem.File.Type;
};

/** Every entry below a directory, at any depth, or none when the directory does not exist. */
function listTreeEntries(input: {
	readonly fileSystem: FileSystem.FileSystem;
	readonly path: Path.Path;
	readonly dir: string;
}): Effect.Effect<ReadonlyArray<TreeEntry>, PlatformError> {
	const { fileSystem, path, dir } = input;
	return fileSystem.readDirectory(dir, { recursive: true }).pipe(
		Effect.catchReason("PlatformError", "NotFound", () => {
			return Effect.succeed<ReadonlyArray<string>>([]);
		}),
		Effect.flatMap((names) => {
			return Effect.forEach(
				names,
				(name) => {
					return fileSystem.stat(path.join(dir, name)).pipe(
						Effect.map((info): TreeEntry => {
							return {
								name: name.split(path.sep).join("/"),
								type: info.type,
							};
						}),
					);
				},
				{ concurrency: 16 },
			);
		}),
	);
}

/** Every directory that holds one of the files, at any depth, by its POSIX path. */
function listParentDirs(fileNames: Iterable<string>): ReadonlySet<string> {
	const dirs = new Set<string>();
	for (const fileName of fileNames) {
		const parts = fileName.split("/");
		for (let end = 1; end < parts.length; end++) {
			dirs.add(parts.slice(0, end).join("/"));
		}
	}
	return dirs;
}

/**
 * The differences between a staged `public` and the files that its metadata
 * lists: the entries that the metadata does not list, such as a file that a
 * plugin wrote in `writeBundle` or an empty directory, and the listed files
 * that `public` does not hold.
 */
function findPublicDirDifferences(input: {
	readonly entries: ReadonlyArray<TreeEntry>;
	readonly listedFileNames: ReadonlyArray<string>;
}) {
	const listed = new Set(input.listedFileNames);
	const listedDirs = listParentDirs(listed);
	const unlisted = input.entries.flatMap((entry) => {
		return (entry.type === "File" && listed.has(entry.name)) ||
			(entry.type === "Directory" && listedDirs.has(entry.name))
			? []
			: [entry.name];
	});
	const files = new Set(
		input.entries.flatMap((entry) => {
			return entry.type === "File" ? [entry.name] : [];
		}),
	);
	const missing = input.listedFileNames.filter((name) => !files.has(name));
	return { unlisted, missing };
}

/**
 * Removes every staging directory that a build left in a build directory when
 * it ended without closing its scope, such as a build that received
 * `SIGKILL`. Only the owner of the build directory's lock creates a staging
 * directory, through `createStagedBuildDir`, and an owner removes its staging
 * directory when its scope closes, before it releases the lock. The caller
 * owns the lock, so every staging directory that exists when it starts
 * belongs to an owner that ended without its cleanup.
 */
function removeAbandonedStagingDirs(input: {
	readonly stagingRoot: string;
}): Effect.Effect<void, never, FileSystem.FileSystem | Path.Path> {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const path = yield* Path.Path;
		const abandoned = yield* readDirIfPresent({
			fileSystem,
			dir: input.stagingRoot,
		});
		yield* Effect.forEach(
			abandoned,
			(entry) => {
				return fileSystem.remove(path.join(input.stagingRoot, entry), {
					recursive: true,
					force: true,
				});
			},
			{ discard: true },
		);
	}).pipe(
		Effect.catch((cause) => {
			return Effect.logWarning(
				`Could not remove the abandoned staging directories in ${input.stagingRoot}`,
				cause,
			);
		}),
	);
}

/**
 * Creates a staging directory in a build directory that this process owns,
 * with an empty `public` directory for the staged build. The staging
 * directory's finalizer must run before the finalizer of the ownership, which
 * holds when both belong to one scope and the ownership was acquired first, so
 * that the owner removes its staging directory before it releases the lock.
 */
export function createStagedBuildDir(input: {
	readonly owned: OwnedBuildDir;
}): Effect.Effect<
	StagedBuildDir,
	OutputOwnershipIssue,
	Scope.Scope | FileSystem.FileSystem | Path.Path
> {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const path = yield* Path.Path;
		const { buildDir } = input.owned;
		const stagingRoot = path.join(
			buildDir,
			ARTIFACT_PROTOCOL.stagingDirName,
		);
		function toStagingIssue(cause: PlatformError): OutputOwnershipIssue {
			return {
				_tag: "OutputOwnershipIssue",
				dir: buildDir,
				msg: `Cannot create a staging directory in ${buildDir}: ${cause.message}`,
				cause,
			};
		}
		yield* removeAbandonedStagingDirs({ stagingRoot });
		const stagingDir = yield* Effect.acquireRelease(
			fileSystem.makeDirectory(stagingRoot, { recursive: true }).pipe(
				Effect.andThen(
					fileSystem.makeTempDirectory({
						directory: stagingRoot,
					}),
				),
				Effect.mapError(toStagingIssue),
			),
			(dir) => {
				// The staging directory goes, and so does `staging` when it is
				// empty.
				return fileSystem
					.remove(dir, { recursive: true, force: true })
					.pipe(
						Effect.andThen(
							readDirIfPresent({
								fileSystem,
								dir: stagingRoot,
							}),
						),
						Effect.flatMap((remaining) => {
							return remaining.length === 0
								? fileSystem.remove(stagingRoot, {
										recursive: true,
										force: true,
									})
								: Effect.void;
						}),
						Effect.catch((cause) => {
							return Effect.logWarning(
								`Could not remove the staging directory ${dir}`,
								cause,
							);
						}),
					);
			},
		);
		const staged = getBuildDirPaths({ path, buildDir: stagingDir });
		const published = getBuildDirPaths({ path, buildDir });
		yield* fileSystem
			.makeDirectory(staged.publicDir)
			.pipe(Effect.mapError(toStagingIssue));
		function publish(publication: {
			readonly metadata: WrittenBuildMetadata;
			readonly check: CheckedBuildDir;
		}) {
			return Effect.gen(function* () {
				if (
					publication.metadata.filePath !==
						staged.buildMetadataFilePath ||
					publication.check.buildDir !== stagingDir
				) {
					return yield* Effect.die(
						new Error(
							`Vorma published ${stagingDir} with the metadata of ${publication.metadata.filePath} and the check run of ${publication.check.buildDir}`,
						),
					);
				}
				// The published `public` must be a directory of the build directory
				// itself, because publication moves files into it and removes
				// files from it.
				yield* checkOutputPath({
					filePath: published.publicDir,
					rootDir: buildDir,
				});
				const stagedEntries = yield* listTreeEntries({
					fileSystem,
					path,
					dir: staged.publicDir,
				});
				const { unlisted, missing } = findPublicDirDifferences({
					entries: stagedEntries,
					listedFileNames:
						publication.metadata.staticPublicAssetFileNames,
				});
				if (unlisted.length > 0 || missing.length > 0) {
					return yield* Effect.fail({
						_tag: "OutputOwnershipIssue" as const,
						dir: buildDir,
						msg: [
							`Vorma does not publish the build to ${buildDir}: its public directory does not hold exactly the files that its metadata lists.`,
							...(unlisted.length === 0
								? []
								: [
										`Entries that the metadata does not list, such as files that a plugin wrote in writeBundle: ${unlisted.join(", ")}`,
									]),
							...(missing.length === 0
								? []
								: [
										`Files that the metadata lists and the public directory does not hold: ${missing.join(", ")}`,
									]),
						].join("\n"),
					});
				}
				const stagedFileNames =
					publication.metadata.staticPublicAssetFileNames;
				yield* Effect.forEach(
					stagedFileNames,
					(fileName) => {
						const target = path.join(published.publicDir, fileName);
						return fileSystem
							.makeDirectory(path.dirname(target), {
								recursive: true,
							})
							.pipe(
								Effect.andThen(
									fileSystem.rename(
										path.join(staged.publicDir, fileName),
										target,
									),
								),
							);
					},
					{ concurrency: 16, discard: true },
				);
				// This rename publishes the build: the metadata lists exactly the
				// staged files, and all of them are in place.
				yield* fileSystem.rename(
					publication.metadata.filePath,
					published.buildMetadataFilePath,
				);
				// What the staged build does not have goes: the other files and
				// directories of `public`, and every entry of the build directory
				// that Vorma does not write.
				const retainedFiles = new Set(stagedFileNames);
				const retainedDirs = listParentDirs(retainedFiles);
				const publishedEntries = yield* listTreeEntries({
					fileSystem,
					path,
					dir: published.publicDir,
				});
				const removedNames = new Set(
					publishedEntries.flatMap((entry) => {
						return (entry.type === "File" &&
							retainedFiles.has(entry.name)) ||
							(entry.type === "Directory" &&
								retainedDirs.has(entry.name))
							? []
							: [entry.name];
					}),
				);
				yield* Effect.forEach(
					// A removed directory takes the entries below it with it.
					[...removedNames].filter((name) => {
						return ![...listParentDirs([name])].some((dir) => {
							return removedNames.has(dir);
						});
					}),
					(name) => {
						return fileSystem.remove(
							path.join(published.publicDir, ...name.split("/")),
							{ recursive: true, force: true },
						);
					},
					{ concurrency: 16, discard: true },
				);
				yield* Effect.forEach(
					yield* fileSystem.readDirectory(buildDir),
					(entry) => {
						return VORMA_BUILD_DIR_ENTRY_NAMES.has(entry)
							? Effect.void
							: fileSystem.remove(path.join(buildDir, entry), {
									recursive: true,
									force: true,
								});
					},
					{ discard: true },
				);
			}).pipe(
				Effect.provideService(FileSystem.FileSystem, fileSystem),
				Effect.provideService(Path.Path, path),
				Effect.uninterruptible,
				Effect.mapError((cause): OutputOwnershipIssue => {
					return cause._tag === "OutputOwnershipIssue"
						? cause
						: {
								_tag: "OutputOwnershipIssue",
								dir: buildDir,
								msg: `Cannot publish the completed build to ${buildDir}: ${cause.message}`,
								cause,
							};
				}),
			);
		}
		return { stagingDir, publish };
	});
}

/** The build directory paths of a staged build, inside its staging directory. */
export function getStagedBuildDirPaths(input: {
	readonly path: Path.Path;
	readonly staged: StagedBuildDir;
}) {
	return getBuildDirPaths({
		path: input.path,
		buildDir: input.staged.stagingDir,
	});
}

/** A build directory that this process owns until the scope that acquired it closes. */
export type OwnedBuildDir = {
	readonly _tag: "OwnedBuildDir";
	/** The canonical path of the build directory. */
	readonly buildDir: string;
};

/** The message for a lock that another process holds, with its owner record as diagnostic data. */
function describeContendedLock(input: {
	readonly kind: "build" | "dev";
	readonly lockPath: string;
	readonly error: Extract<ProcessLockError, { readonly kind: "contended" }>;
}): string {
	const owner =
		input.error.recordedOwnerRecord === undefined
			? undefined
			: Option.getOrUndefined(
					Schema.decodeUnknownOption(Owner)(
						input.error.recordedOwnerRecord,
					),
				);
	return [
		`Cannot start ${input.kind}: the build directory is locked.`,
		`Lock: ${input.lockPath}`,
		`Recorded owner PID: ${owner?.pid ?? "unavailable"}`,
		`Recorded operation: ${owner?.kind ?? "unavailable"}`,
		"Wait for the owning process to finish.",
	].join("\n");
}

/**
 * Owns the build directory until the build or development scope closes,
 * through the process lock of `primitives-ts` in the build directory's lock
 * directory. The operating system releases the lock when this process ends
 * for any reason, so a lock of a process that ended needs no cleanup. When the
 * scope closes, the owner removes the build directory's `public` directory if
 * the build directory holds no build metadata, and then releases its lock.
 * The lock directory stays in the build directory.
 */
export function acquireOutputOwnership(input: {
	readonly buildDir: string;
	readonly kind: "build" | "dev";
}): Effect.Effect<
	OwnedBuildDir,
	OutputOwnershipIssue,
	Scope.Scope | FileSystem.FileSystem | Path.Path
> {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const path = yield* Path.Path;
		const dir = yield* canonicalizeOutputPath(input.buildDir);
		yield* checkBuildDirContents({ fileSystem, path, dir });
		yield* fileSystem.makeDirectory(dir, { recursive: true });
		const lockPath = path.join(dir, ARTIFACT_PROTOCOL.lockDirName);
		yield* fileSystem.makeDirectory(lockPath, { mode: 0o700 }).pipe(
			Effect.catchReason("PlatformError", "AlreadyExists", () => {
				return Effect.void;
			}),
		);
		if ((yield* fileSystem.stat(lockPath)).type !== "Directory") {
			return yield* Effect.fail({
				_tag: "OutputOwnershipIssue" as const,
				dir,
				msg: `Vorma refuses to use ${lockPath} as its lock: it is not a directory, and no Vorma version creates it. Remove it if no other program uses it.`,
			});
		}
		const owner: LockOwner = { pid, kind: input.kind, dir };
		// The acquisition, which writes the owner record, completes before
		// interruption can run the release.
		yield* Effect.acquireRelease(
			Effect.promise(() => {
				return tryAcquireProcessLock({
					dir: lockPath,
					ownerRecord: Schema.encodeSync(Owner)(owner),
				});
			}).pipe(
				Effect.flatMap((acquired) => {
					if (acquired.kind === "ok") {
						return Effect.succeed(acquired.value);
					}
					return Effect.fail({
						_tag: "OutputOwnershipIssue" as const,
						dir,
						msg:
							acquired.error.kind === "contended"
								? describeContendedLock({
										kind: input.kind,
										lockPath,
										error: acquired.error,
									})
								: `Cannot lock the build directory ${dir}: ${acquired.error.msg}`,
						cause: acquired.error,
					});
				}),
			),
			(lock) => {
				return removeUnpublishedPublicDir({
					fileSystem,
					path,
					dir,
				}).pipe(
					Effect.catch((cause) => {
						return Effect.logError(
							`Could not remove the unpublished public directory of ${dir}`,
							cause,
						);
					}),
					Effect.andThen(Effect.promise(() => lock.release())),
					Effect.flatMap((released) => {
						return released.kind === "ok"
							? Effect.void
							: Effect.logError(
									`Could not release the Vorma lock ${lockPath} completely`,
									released.error,
								);
					}),
				);
			},
		);
		return { _tag: "OwnedBuildDir", buildDir: dir } satisfies OwnedBuildDir;
	}).pipe(
		Effect.mapError((cause): OutputOwnershipIssue => {
			return cause._tag === "OutputOwnershipIssue"
				? cause
				: {
						_tag: "OutputOwnershipIssue",
						dir: input.buildDir,
						msg: `Cannot take ownership of the build directory ${input.buildDir}: ${describeCause(cause)}`,
						cause,
					};
		}),
	);
}
