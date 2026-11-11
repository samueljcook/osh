import { constants } from "node:fs";
import { open, readdir, rename, unlink, writeFile } from "node:fs/promises";
import nodePath from "node:path";
import {
	callWithResultAsync,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../../../results/index.ts";
import { createRandomId, readErrorCode } from "./ownership.ts";

/*
Reads, writes, and removes the diagnostic owner record in the lock directory.
The owner writes this record after acquiring the lock and removes it when
releasing the lock. Because only an owner writes or removes the record and at
most one participant owns the lock at a time, writers never contend with each
other. A reader can observe the record of an earlier owner that ended without
releasing the lock, or undefined before the current owner writes its record.
*/

export const OWNER_RECORD_FILE_NAME = "owner.record";
/** Specifies the maximum UTF-8 byte length of an owner record. */
export const OWNER_RECORD_SIZE_LIMIT = 4096;
const RECORD_TEMPORARY_NAME_PATTERN = /^record-[0-9a-f]{32}\.tmp$/;

const NO_FOLLOW_FLAG = process.platform === "win32" ? 0 : constants.O_NOFOLLOW;

/**
 * Replaces the owner record through a temporary file and one rename, after it
 * removes the temporary files of earlier owners that ended while they wrote
 * their records.
 */
export async function writeOwnerRecord(input: {
	readonly dirPath: string;
	readonly ownerRecord: string;
}): Promise<Result<void, unknown>> {
	const names = await callWithResultAsync(() => readdir(input.dirPath));
	if (names.kind === "error") {
		return names;
	}
	for (const name of names.value) {
		if (RECORD_TEMPORARY_NAME_PATTERN.test(name)) {
			const removed = await callWithResultAsync(() =>
				unlink(nodePath.join(input.dirPath, name)),
			);
			if (
				removed.kind === "error" &&
				readErrorCode(removed.error) !== "ENOENT"
			) {
				return removed;
			}
		}
	}
	const id = createRandomId();
	if (id.kind === "error") {
		return id;
	}
	const temporaryPath = nodePath.join(
		input.dirPath,
		`record-${id.value}.tmp`,
	);
	const written = await callWithResultAsync(() => {
		return writeFile(temporaryPath, input.ownerRecord, {
			flag:
				constants.O_WRONLY |
				constants.O_CREAT |
				constants.O_EXCL |
				NO_FOLLOW_FLAG,
			mode: 0o666,
		});
	});
	const renamed =
		written.kind === "error"
			? written
			: await callWithResultAsync(() => {
					return rename(
						temporaryPath,
						nodePath.join(input.dirPath, OWNER_RECORD_FILE_NAME),
					);
				});
	if (renamed.kind === "error") {
		await callWithResultAsync(() => unlink(temporaryPath));
	}
	return renamed;
}

/** Removes the owner record; a missing record is not an error. */
export async function removeOwnerRecord(
	dirPath: string,
): Promise<Result<void, unknown>> {
	const removed = await callWithResultAsync(() =>
		unlink(nodePath.join(dirPath, OWNER_RECORD_FILE_NAME)),
	);
	return removed.kind === "error" && readErrorCode(removed.error) === "ENOENT"
		? createSuccessResult(undefined)
		: removed;
}

/** Describes why `readOwnerRecord` could not return an existing owner record as text. */
export type UnreadableOwnerRecord =
	| { readonly kind: "oversized" }
	| { readonly kind: "invalid-utf8" }
	| { readonly kind: "io"; readonly cause: unknown };

/** Reads a bounded regular-file owner record, or returns undefined when no record exists. */
export async function readOwnerRecord(
	dirPath: string,
): Promise<Result<string | undefined, UnreadableOwnerRecord>> {
	const opened = await callWithResultAsync(() => {
		return open(
			nodePath.join(dirPath, OWNER_RECORD_FILE_NAME),
			constants.O_RDONLY |
				NO_FOLLOW_FLAG |
				(process.platform === "win32" ? 0 : constants.O_NONBLOCK),
		);
	});
	if (opened.kind === "error") {
		return readErrorCode(opened.error) === "ENOENT"
			? createSuccessResult(undefined)
			: createErrorResult({ kind: "io", cause: opened.error });
	}
	const handle = opened.value;
	const inspected = await callWithResultAsync(() => handle.stat());
	if (inspected.kind === "error" || !inspected.value.isFile()) {
		await callWithResultAsync(() => handle.close());
		return createErrorResult({
			kind: "io",
			cause:
				inspected.kind === "error"
					? inspected.error
					: new Error("The owner record must be a regular file"),
		});
	}
	const bytes = new Uint8Array(OWNER_RECORD_SIZE_LIMIT + 1);
	const read = await callWithResultAsync(async () => {
		let length = 0;
		for (;;) {
			const { bytesRead } = await handle.read(
				bytes,
				length,
				bytes.length - length,
				null,
			);
			length += bytesRead;
			if (bytesRead === 0 || length === bytes.length) {
				return length;
			}
		}
	});
	await callWithResultAsync(() => handle.close());
	if (read.kind === "error") {
		return createErrorResult({ kind: "io", cause: read.error });
	}
	if (read.value > OWNER_RECORD_SIZE_LIMIT) {
		return createErrorResult({ kind: "oversized" });
	}
	const decoded = await callWithResultAsync(() => {
		return new TextDecoder("utf-8", { fatal: true }).decode(
			bytes.subarray(0, read.value),
		);
	});
	return decoded.kind === "error"
		? createErrorResult({ kind: "invalid-utf8" })
		: createSuccessResult(decoded.value);
}
