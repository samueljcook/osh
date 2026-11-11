import { lstat, realpath } from "node:fs/promises";
import nodePath from "node:path";
import type { Result } from "../../../results/index.ts";
import {
	checkPathContainedWithFs,
	resolveCanonicalPathWithFs,
	type ContainmentFileSystem,
} from "./containment.ts";

/*
Checks whether a path names a directory or a location below that directory.
`testPathContained` compares the two paths as text. `resolveCanonicalPath` and
`checkPathContained` inspect the file system: because a symbolic link can make
a lexically contained path point outside the directory, those operations
resolve symbolic links with `realpath`, including when trailing path components
do not exist yet. Every file-system result reflects the file system at the
moment of inspection: if someone modifies a symbolic link after inspection, a
checked path can resolve to a different location, so untrusted users must not
have write access to a directory that contains checked paths.
*/

const NODE_CONTAINMENT_FS: ContainmentFileSystem = {
	path: nodePath,
	readEntryKind: async (filePath) => {
		const stats = await lstat(filePath);
		if (stats.isSymbolicLink()) {
			return "symbolic-link";
		}
		if (stats.isDirectory()) {
			return "directory";
		}
		return "other";
	},
	realpath,
};

/**
 * Provides the path operations that `testPathContained` requires. `node:path`,
 * its `posix` and `win32` namespaces, and Effect's `Path` service satisfy this
 * type. The supplied operations determine path syntax: `node:path.posix`
 * compares POSIX paths on every platform.
 */
export type PathContainmentOperations = Pick<
	typeof nodePath,
	"relative" | "isAbsolute"
> & { readonly sep: string };

/**
 * Tests whether `filePath` names `dir` or a path below `dir`, by comparing the
 * two paths as text through `path.relative`. The test reads no file system:
 * it does not resolve symbolic links, and it does not ignore case differences
 * that a file system ignores. `path.relative` resolves a relative path against
 * the working directory. A name that starts with two dots, such as `..data`,
 * is a name below `dir`. On Windows, a path on another drive is outside `dir`.
 */
export function testPathContained(input: {
	readonly path: PathContainmentOperations;
	readonly dir: string;
	readonly filePath: string;
}): boolean {
	const relativePath = input.path.relative(input.dir, input.filePath);
	return (
		relativePath === "" ||
		(!input.path.isAbsolute(relativePath) &&
			relativePath !== ".." &&
			!relativePath.startsWith(`..${input.path.sep}`))
	);
}

/** Describes why canonical path resolution failed. */
export type CanonicalPathError =
	| {
			/** An existing entry on the path is a symbolic link with a missing target. */
			readonly kind: "dangling-symbolic-link";
			readonly linkPath: string;
			readonly msg: string;
	  }
	| {
			/** A file-system call failed for a reason other than a missing entry. */
			readonly kind: "io";
			readonly path: string;
			readonly msg: string;
			readonly cause: unknown;
	  };

/**
 * Resolves `filePath` against the working directory and returns the canonical
 * absolute path: the `realpath` of the longest existing ancestor, followed by
 * the components below it that do not exist. The result is the path that a
 * file created at `filePath` would have. An existing entry on the path that is
 * a symbolic link to a missing target returns a `dangling-symbolic-link` error
 * because creating the target later would change the location that the path
 * names. A missing entry is not an error; any other failure of `realpath` or
 * `lstat`, such as a component that is a regular file, is an `io` error.
 */
export async function resolveCanonicalPath(input: {
	readonly filePath: string;
}): Promise<Result<string, CanonicalPathError>> {
	return resolveCanonicalPathWithFs({
		filePath: input.filePath,
		fs: NODE_CONTAINMENT_FS,
	});
}

/** Describes why `checkPathContained` rejected or could not resolve a path. */
export type PathContainmentError =
	| {
			/** The path is outside the directory as text. */
			readonly kind: "outside-dir";
			readonly msg: string;
	  }
	| {
			/** An entry between the directory and the path is a symbolic link. */
			readonly kind: "symbolic-link-below-dir";
			readonly msg: string;
	  }
	| CanonicalPathError;

/**
 * Checks that `filePath` names `dir` or a location below `dir` and that no
 * existing entry from `dir` down to `filePath` is a symbolic link so that an
 * operation on `filePath` acts on the location inside `dir` that its text
 * names. Symbolic links above `dir`, and `dir` itself, may be links. Both
 * paths are resolved against the working directory and with
 * `resolveCanonicalPath`, so missing trailing components are allowed. Existing
 * components below `dir` are inspected with `lstat`, so case-insensitive file
 * systems accept existing subdirectories with different on-disk casing while
 * rejecting any symbolic link below `dir`, even when its target is inside
 * `dir`.
 */
export async function checkPathContained(input: {
	readonly dir: string;
	readonly filePath: string;
}): Promise<Result<void, PathContainmentError>> {
	return checkPathContainedWithFs({
		dir: input.dir,
		filePath: input.filePath,
		fs: NODE_CONTAINMENT_FS,
	});
}
