import type nodePath from "node:path";
import {
	callWithResultAsync,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../../../results/index.ts";
import type {
	CanonicalPathError,
	PathContainmentError,
	PathContainmentOperations,
} from "./index.ts";
import { testPathContained } from "./index.ts";

export type ContainmentPathOperations = PathContainmentOperations &
	Pick<typeof nodePath, "basename" | "dirname" | "join" | "resolve">;

export type ContainmentEntryKind = "symbolic-link" | "directory" | "other";

export type ContainmentFileSystem = {
	readonly path: ContainmentPathOperations;
	readonly readEntryKind: (filePath: string) => Promise<ContainmentEntryKind>;
	readonly realpath: (filePath: string) => Promise<string>;
};

export async function resolveCanonicalPathWithFs(input: {
	readonly filePath: string;
	readonly fs: ContainmentFileSystem;
}): Promise<Result<string, CanonicalPathError>> {
	const { fs } = input;
	let current = fs.path.resolve(input.filePath);
	const missingComponents: Array<string> = [];
	for (;;) {
		const resolved = await callWithResultAsync(() => fs.realpath(current));
		if (resolved.kind === "ok") {
			return createSuccessResult(
				fs.path.join(resolved.value, ...missingComponents),
			);
		}
		if (readErrorCode(resolved.error) !== "ENOENT") {
			return createErrorResult(
				toIoError({ path: current, cause: resolved.error }),
			);
		}
		// `realpath` fails with ENOENT both for a missing entry and for a
		// link to a missing target; only the first case continues upward.
		const entry = await callWithResultAsync(() =>
			fs.readEntryKind(current),
		);
		if (entry.kind === "ok") {
			if (entry.value !== "symbolic-link") {
				continue;
			}
			return createErrorResult({
				kind: "dangling-symbolic-link",
				linkPath: current,
				msg: `${current} is a symbolic link with a missing target`,
			});
		}
		if (readErrorCode(entry.error) !== "ENOENT") {
			return createErrorResult(
				toIoError({ path: current, cause: entry.error }),
			);
		}
		const parent = fs.path.dirname(current);
		if (parent === current) {
			return createErrorResult(
				toIoError({ path: current, cause: resolved.error }),
			);
		}
		missingComponents.unshift(fs.path.basename(current));
		current = parent;
	}
}

export async function checkPathContainedWithFs(input: {
	readonly dir: string;
	readonly filePath: string;
	readonly fs: ContainmentFileSystem;
}): Promise<Result<void, PathContainmentError>> {
	const { fs } = input;
	const dir = fs.path.resolve(input.dir);
	const filePath = fs.path.resolve(input.filePath);
	if (!testPathContained({ path: fs.path, dir, filePath })) {
		return createErrorResult({
			kind: "outside-dir",
			msg: `${filePath} is outside ${dir}`,
		});
	}
	const [canonicalDir, canonicalFilePath] = await Promise.all([
		resolveCanonicalPathWithFs({ filePath: dir, fs }),
		resolveCanonicalPathWithFs({ filePath, fs }),
	]);
	if (canonicalDir.kind === "error") {
		return canonicalDir;
	}
	if (canonicalFilePath.kind === "error") {
		return canonicalFilePath;
	}
	const dirEntry = await callWithResultAsync(() =>
		fs.readEntryKind(canonicalDir.value),
	);
	if (dirEntry.kind === "error") {
		if (readErrorCode(dirEntry.error) !== "ENOENT") {
			return createErrorResult(
				toIoError({ path: canonicalDir.value, cause: dirEntry.error }),
			);
		}
	} else if (dirEntry.value !== "directory") {
		return createErrorResult(
			toIoError({
				path: canonicalDir.value,
				cause: new Error(`${canonicalDir.value} is not a directory`),
			}),
		);
	}
	const relativePath = fs.path.relative(dir, filePath);
	if (relativePath !== "") {
		const segments = relativePath.split(fs.path.sep);
		let current = canonicalDir.value;
		for (let index = 0; index < segments.length; index++) {
			current = fs.path.join(current, segments[index]!);
			const entry = await callWithResultAsync(() =>
				fs.readEntryKind(current),
			);
			if (entry.kind === "error") {
				if (readErrorCode(entry.error) === "ENOENT") {
					break;
				}
				return createErrorResult(
					toIoError({ path: current, cause: entry.error }),
				);
			}
			if (entry.value === "symbolic-link") {
				return createErrorResult({
					kind: "symbolic-link-below-dir",
					msg: `${filePath} passes through a symbolic link below ${dir}; it resolves to ${canonicalFilePath.value}`,
				});
			}
			if (index < segments.length - 1 && entry.value !== "directory") {
				return createErrorResult(
					toIoError({
						path: current,
						cause: new Error(`${current} is not a directory`),
					}),
				);
			}
		}
	}
	if (
		!testPathContained({
			path: fs.path,
			dir: canonicalDir.value,
			filePath: canonicalFilePath.value,
		})
	) {
		return createErrorResult({
			kind: "symbolic-link-below-dir",
			msg: `${filePath} passes through a symbolic link below ${dir}; it resolves to ${canonicalFilePath.value}`,
		});
	}
	return createSuccessResult(undefined);
}

function readErrorCode(error: unknown): string | undefined {
	return typeof error === "object" &&
		error !== null &&
		"code" in error &&
		typeof error.code === "string"
		? error.code
		: undefined;
}

function toIoError(input: {
	readonly path: string;
	readonly cause: unknown;
}): CanonicalPathError {
	return {
		kind: "io",
		path: input.path,
		msg: `Cannot resolve ${input.path}: ${input.cause instanceof Error ? input.cause.message : String(input.cause)}`,
		cause: input.cause,
	};
}
