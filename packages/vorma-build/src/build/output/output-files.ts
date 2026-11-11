import { Effect, Result } from "effect";
import * as FileSystem from "effect/FileSystem";
import * as Path from "effect/Path";
import {
	writeFileAtomically,
	type AtomicWriteError,
} from "primitives-ts/fs/atomic-writes/node";
import { GENERATED_SOURCE_HEADER } from "../generation/render-source-text.ts";
import { describeCause } from "../issues.ts";

export type GeneratedSourceConflictIssue = {
	readonly _tag: "GeneratedSourceConflictIssue";
	readonly filePath: string;
	readonly msg: string;
};

/** The atomic replacement of a generated source file failed. */
export type GeneratedSourceWriteIssue = {
	readonly _tag: "GeneratedSourceWriteIssue";
	readonly filePath: string;
	readonly msg: string;
	readonly cause: unknown;
};

/** The reason of a failed atomic write, without the path that the message of the build names. */
export function describeAtomicWriteError(error: AtomicWriteError): string {
	return (error.kind === "before-publication" ||
		error.kind === "published-but-not-durable") &&
		error.cause !== undefined
		? describeCause(error.cause)
		: error.msg;
}

/**
 * Atomically replaces generated source that TypeScript and build tools can
 * read. Existing files must have Vorma's generated-source header. A file that
 * already holds the source is left untouched, so editors and watchers see no
 * change. The write goes through a temporary file beside the output file,
 * which `testAtomicWriteTemporaryFileName` of `primitives-ts` recognizes.
 */
export function writeOutputFile(input: {
	readonly filePath: string;
	readonly source: string;
}) {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const path = yield* Path.Path;
		const existing = yield* fileSystem
			.readFileString(input.filePath)
			.pipe(Effect.result);
		if (Result.isFailure(existing)) {
			if (existing.failure.reason._tag !== "NotFound") {
				return yield* Effect.fail(existing.failure);
			}
		} else if (
			existing.success.split(/\r?\n/, 1)[0] !== GENERATED_SOURCE_HEADER
		) {
			return yield* Effect.fail({
				_tag: "GeneratedSourceConflictIssue",
				filePath: input.filePath,
				msg: `Refusing to overwrite ${input.filePath}: it is not a Vorma-generated source file. Choose another generated types path.`,
			} satisfies GeneratedSourceConflictIssue);
		}
		yield* fileSystem.makeDirectory(path.dirname(input.filePath), {
			recursive: true,
		});
		const written = yield* Effect.promise(() => {
			return writeFileAtomically({
				filePath: input.filePath,
				bytes: new TextEncoder().encode(input.source),
				permissions: { kind: "platform-default" },
				replacement: "if-changed",
				durability: "none",
			});
		});
		if (written.kind === "error") {
			return yield* Effect.fail({
				_tag: "GeneratedSourceWriteIssue",
				filePath: input.filePath,
				msg: `Could not write ${input.filePath}: ${describeAtomicWriteError(written.error)}`,
				cause: written.error,
			} satisfies GeneratedSourceWriteIssue);
		}
	}).pipe(Effect.uninterruptible);
}
