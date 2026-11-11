import { existsSync, lstatSync } from "node:fs";
import { lstat } from "node:fs/promises";
import * as nodePath from "node:path";
import { watch } from "chokidar";
import { Effect, Equal, Result, ScopedRef } from "effect";
import { testAtomicWriteTemporaryFileName } from "primitives-ts/fs/atomic-writes/node";
import { testPathContained } from "primitives-ts/fs/path-containment/node";
import { normalizePath } from "vite";
import { describeCause } from "../issues.ts";
import { createDevChanges } from "./changes.ts";

/** The native watcher could not start. */
export type DevWatchIssue = {
	readonly _tag: "DevWatchIssue";
	readonly msg: string;
	readonly cause: unknown;
};

const IGNORED_DIR_NAMES = new Set(["node_modules", "target", ".git"]);

// Chokidar v5 throttles repeated `change` events on one path for 50 ms. A
// trailing stat check after that window detects rapid follow-up writes without
// delaying the initial event.
const CHOKIDAR_RECHECK_DELAY_MILLIS = 65;

function readFileSignatureSync(filePath: string): string {
	const stat = Result.try(() =>
		lstatSync(filePath, { bigint: true, throwIfNoEntry: false }),
	);
	if (Result.isFailure(stat) || stat.success === undefined) {
		return "missing";
	}
	return `${stat.success.ino}:${stat.success.size}:${stat.success.mtimeNs}:${stat.success.ctimeNs}`;
}

function readFileSignature(filePath: string): Promise<string> {
	return lstat(filePath, { bigint: true }).then(
		(stat) => `${stat.ino}:${stat.size}:${stat.mtimeNs}:${stat.ctimeNs}`,
		() => "missing",
	);
}

/**
 * Whether a path is inside a directory named `node_modules`, `target`, or
 * `.git` at any level below the configuration directory. Vorma's watcher and
 * Vite's watcher both ignore these paths. Directories above the configuration
 * directory do not count, so a project inside a directory with one of these
 * names is still watched.
 */
export function testIgnoredDirPath(input: {
	readonly configDir: string;
	readonly filePath: string;
}) {
	return nodePath
		.relative(input.configDir, input.filePath)
		.split(/[/\\]/)
		.some((segment) => IGNORED_DIR_NAMES.has(segment));
}

/** Whether a path is a temporary file through which Vorma atomically writes a file beside a generated path. */
function testTemporaryOutputPath(input: {
	readonly generatedPath: string;
	readonly filePath: string;
}) {
	return (
		nodePath.dirname(input.filePath) ===
			nodePath.dirname(input.generatedPath) &&
		testAtomicWriteTemporaryFileName(nodePath.basename(input.filePath))
	);
}

/** Application watching survives Vite restarts and failed replacement builds. */
export function createDevWatcher(input: {
	readonly configFilePath: string;
	readonly generatedPaths: ReadonlyArray<string>;
}) {
	return Effect.gen(function* () {
		const changes = yield* createDevChanges();
		type WatchPlan = {
			readonly paths: ReadonlyArray<string>;
			readonly generatedPaths: ReadonlyArray<string>;
		};
		const configDir = nodePath.dirname(input.configFilePath);
		function testIgnoredPath(filePath: string) {
			return testIgnoredDirPath({ configDir, filePath });
		}
		function acquireWatcher(plan: WatchPlan) {
			return Effect.gen(function* () {
				const roots = new Set(plan.paths);
				/** The watcher reports changes once it has scanned every path of the plan. */
				let phase: "starting" | "watching" | "closed" = "starting";
				const recheckTimers = new Map<
					string,
					ReturnType<typeof setTimeout>
				>();
				const watcher = yield* Effect.acquireRelease(
					Effect.sync(() => {
						return watch([...plan.paths], {
							atomic: false,
							ignoreInitial: true,
							ignored: (filePath) => {
								return (
									testIgnoredPath(filePath) ||
									plan.generatedPaths.some(
										(generatedPath) => {
											return (
												testPathContained({
													path: nodePath,
													dir: generatedPath,
													filePath,
												}) ||
												testTemporaryOutputPath({
													generatedPath,
													filePath,
												})
											);
										},
									)
								);
							},
						});
					}),
					(watcher) => {
						return Effect.promise(() => {
							phase = "closed";
							for (const timer of recheckTimers.values()) {
								clearTimeout(timer);
							}
							recheckTimers.clear();
							return watcher.close();
						});
					},
				);
				/**
				 * The directories of the plan that were removed and that the
				 * watcher watches again until they return. Chokidar stops
				 * watching a removed directory that the plan names, and under
				 * load it can report only the removal of the directory's files.
				 * Watching the path again reports the directory and its files
				 * when they are created again.
				 */
				const removedRoots = new Set<string>();
				watcher.on("all", (event, filePath) => {
					const baseline = readFileSignatureSync(filePath);
					changes.recordChange(filePath);
					const file = normalizePath(filePath);
					if (phase !== "closed") {
						const existingTimer = recheckTimers.get(file);
						if (existingTimer !== undefined) {
							clearTimeout(existingTimer);
						}
						const timer = setTimeout(() => {
							recheckTimers.delete(file);
							if (phase === "closed") {
								return;
							}
							void readFileSignature(filePath).then((current) => {
								if (
									phase !== "closed" &&
									current !== baseline
								) {
									changes.recordChange(filePath);
								}
							});
						}, CHOKIDAR_RECHECK_DELAY_MILLIS);
						recheckTimers.set(file, timer);
					}
					for (const root of roots) {
						const inRoot = testPathContained({
							path: nodePath.posix,
							dir: root,
							filePath: file,
						});
						if (!inRoot) {
							continue;
						}
						if (event === "add" || event === "addDir") {
							removedRoots.delete(root);
						} else if (
							(event === "unlinkDir" ||
								(event === "unlink" && file !== root)) &&
							!removedRoots.has(root) &&
							!existsSync(root)
						) {
							removedRoots.add(root);
							watcher.unwatch(root);
							watcher.add(root);
						}
					}
				});
				// An error during startup fails the watcher acquisition.
				watcher.on("error", (cause) => {
					if (phase === "watching") {
						changes.recordError(cause);
					}
				});
				yield* Effect.callback<void, DevWatchIssue>((resume) => {
					function onReady() {
						phase = "watching";
						resume(Effect.void);
					}
					function onError(cause: unknown) {
						resume(
							Effect.fail({
								_tag: "DevWatchIssue",
								msg: `Vorma could not watch the application's files: ${describeCause(cause)}`,
								cause,
							}),
						);
					}
					watcher.once("ready", onReady);
					watcher.once("error", onError);
					return Effect.sync(() => {
						watcher.off("ready", onReady);
						watcher.off("error", onError);
					});
				});
				return plan;
			});
		}
		const reference = yield* ScopedRef.fromAcquire(
			acquireWatcher({
				paths: [normalizePath(configDir)],
				generatedPaths: input.generatedPaths,
			}),
		);
		return {
			...changes,
			watchPaths(next: WatchPlan) {
				return Effect.gen(function* () {
					const paths = [
						...new Set(
							[configDir, ...next.paths]
								.filter(
									(filePath) => !testIgnoredPath(filePath),
								)
								.map(normalizePath),
						),
					].sort();
					const plan: WatchPlan = {
						paths: paths.filter((filePath) => {
							return !paths.some((dir) => {
								return (
									dir !== filePath &&
									testPathContained({
										path: nodePath.posix,
										dir,
										filePath,
									})
								);
							});
						}),
						generatedPaths: [...next.generatedPaths].sort(),
					};
					if (!Equal.equals(yield* ScopedRef.get(reference), plan)) {
						yield* ScopedRef.set(reference, acquireWatcher(plan));
					}
				});
			},
		};
	});
}
