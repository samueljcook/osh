import type * as Path from "effect/Path";

/** The path operations that a relative POSIX conversion needs. */
export type PosixRelativePathOperations = Pick<Path.Path, "sep" | "relative">;

/**
 * A `path.relative` result with platform separators converted to POSIX
 * separators, for generated module specifiers, output keys, and build
 * identities, which are POSIX even when the build runs on Windows.
 */
export function toPosixRelativePath(input: {
	readonly path: PosixRelativePathOperations;
	readonly from: string;
	readonly to: string;
}): string {
	return input.path
		.relative(input.from, input.to)
		.split(input.path.sep)
		.join("/");
}
