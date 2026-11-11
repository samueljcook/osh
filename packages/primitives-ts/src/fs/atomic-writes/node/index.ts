import { constants, type BigIntStats } from "node:fs";
import { lstat, open, rename, unlink, type FileHandle } from "node:fs/promises";
import nodePath from "node:path";
import { encodeBytes } from "../../../encoding/binary-to-text/index.ts";
import { fillRandomBytes } from "../../../random/secure-bytes/index.ts";
import {
	callWithResultAsync,
	createErrorResult,
	createSuccessResult,
	mapResultError,
	type Result,
} from "../../../results/index.ts";

/*
Replaces a file's contents atomically. The writer writes the complete contents
to a new temporary file in the destination's directory and renames that file
over the destination so that a reader of the destination path observes either
the previous file or the complete new file, never a partial file. If the
process ends at any point, the destination path names either the previous file
or the complete new file, and a temporary file can remain beside it. A
remaining temporary file never has the destination's name;
`testAtomicWriteTemporaryFileName` recognizes temporary file names so that a
watcher can ignore them and cleanup code can remove them while no writer runs.

Atomicity covers process failure. After an operating-system crash or a power
loss, the durability option determines what remains on storage: with `none`, a
file system can retain a renamed file entry without its contents.

Concurrent writers to one destination are not coordinated: each rename replaces
the destination with one complete file, and the final rename determines the
resulting file. An application must coordinate compound updates, such as a read
followed by a write. The destination's directory must not be writable by an
untrusted user, who could replace the destination or the temporary file between
the checks and the rename.

Node provides no rename that refuses an existing destination, so this module
offers no create-only write.
*/

/** Specifies the permissions of the file that a write publishes. */
export type AtomicWritePermissions =
	/**
	 * Applies the permissions that the platform gives a new file: on Unix, mode
	 * `0o666` with the process umask and the directory's default ACL applied. An
	 * unchanged destination keeps its own permissions.
	 */
	| { readonly kind: "platform-default" }
	/** Applies these exact Unix permission bits, from `0o000` to `0o777`; on Windows, `writeFileAtomically` returns `unsupported`. */
	| { readonly kind: "unix"; readonly mode: number };

/** Controls when a write replaces an existing destination. */
export type AtomicWriteReplacement =
	/** Every successful write publishes a new file. */
	| "always"
	/**
	 * A destination with the same bytes, and with the requested Unix mode when
	 * permissions specify one, remains in place with its existing inode and
	 * modification time so that file watchers observe no change.
	 */
	| "if-changed";

/** Specifies the storage synchronization that a write completes before succeeding. */
export type AtomicWriteDurability =
	/** Performs no explicit synchronization. */
	| "none"
	/** The file's contents and metadata are synchronized before the rename. */
	| "file"
	/**
	 * Synchronizes the file before the rename and synchronizes the directory
	 * after the rename so that the directory entry survives a crash. On Windows,
	 * `writeFileAtomically` returns `unsupported` for this option.
	 */
	| "file-and-dir";

/** Describes a successful write. */
export type AtomicWriteOutcome =
	/** `if-changed` found the requested file in place and published nothing. */
	| { readonly kind: "unchanged" }
	/** A complete new file replaced the destination or created it. */
	| { readonly kind: "published" };

/** Describes a failed write; every kind except `published-but-not-durable` published nothing. */
export type AtomicWriteError =
	| { readonly kind: "invalid-options"; readonly msg: string }
	| { readonly kind: "unsupported"; readonly msg: string }
	/** The destination is a symbolic link, a directory, another non-regular entry, or a file with more than one hard link. */
	| { readonly kind: "invalid-destination"; readonly msg: string }
	| {
			readonly kind: "before-publication";
			readonly msg: string;
			readonly cause: unknown;
	  }
	/** The new file is at the destination, but synchronizing its directory failed. */
	| {
			readonly kind: "published-but-not-durable";
			readonly msg: string;
			readonly cause: unknown;
	  };

const TEMPORARY_FILE_NAME_PATTERN = /^\.primitives-[0-9a-f]{32}\.tmp$/;

/** Tests whether a file name, without its directory, is the name of a temporary file that `writeFileAtomically` creates. */
export function testAtomicWriteTemporaryFileName(fileName: string): boolean {
	return TEMPORARY_FILE_NAME_PATTERN.test(fileName);
}

/**
 * Replaces the contents of `filePath` with `bytes` through a temporary file in
 * the same directory and one rename. The directory must exist. An existing
 * destination must be a regular file with one hard link; the write refuses a
 * symbolic link, so it never writes through a link to another location. The
 * new file does not keep the previous file's owner, ACL, or other metadata.
 *
 * The temporary file is created exclusively, without following a symbolic
 * link, and on Unix with mode `0o600` so that no other user can open the file
 * while partial contents are being written. The requested permissions are
 * applied before the rename. With `platform-default`, the function reads the
 * platform's mode for a new file from an empty probe file that it creates and
 * removes, without changing the process umask. A failure before the rename
 * removes the temporary file.
 */
export async function writeFileAtomically(input: {
	readonly filePath: string;
	readonly bytes: Uint8Array;
	readonly permissions: AtomicWritePermissions;
	readonly replacement: AtomicWriteReplacement;
	readonly durability: AtomicWriteDurability;
}): Promise<Result<AtomicWriteOutcome, AtomicWriteError>> {
	const checked = checkOptions(input);
	if (checked.kind === "error") {
		return checked;
	}
	const filePath = nodePath.resolve(input.filePath);
	const dir = nodePath.dirname(filePath);
	if (dir === filePath) {
		return createErrorResult({
			kind: "invalid-destination",
			msg: `${filePath} is a root directory`,
		});
	}
	const existing = await inspectDestination(filePath);
	if (existing.kind === "error") {
		return existing;
	}
	if (input.replacement === "if-changed" && existing.value !== undefined) {
		const unchanged = await testDestinationUnchanged({
			...input,
			filePath,
			dir,
			existing: existing.value,
		});
		if (unchanged.kind === "error" || unchanged.value) {
			return unchanged.kind === "error"
				? unchanged
				: createSuccessResult({ kind: "unchanged" });
		}
	}
	return publishFile({ ...input, filePath, dir });
}

function checkOptions(input: {
	readonly bytes: Uint8Array;
	readonly permissions: AtomicWritePermissions;
	readonly durability: AtomicWriteDurability;
}): Result<void, AtomicWriteError> {
	if (!(input.bytes instanceof Uint8Array)) {
		return createErrorResult({
			kind: "invalid-options",
			msg: "bytes must be a Uint8Array",
		});
	}
	if (input.permissions.kind === "unix") {
		const { mode } = input.permissions;
		if (!Number.isInteger(mode) || mode < 0 || mode > 0o777) {
			return createErrorResult({
				kind: "invalid-options",
				msg: `The Unix mode ${mode} must be an integer from 0o000 to 0o777`,
			});
		}
		if (process.platform === "win32") {
			return createErrorResult({
				kind: "unsupported",
				msg: "Unix permissions are unavailable on Windows",
			});
		}
	}
	if (input.durability === "file-and-dir" && process.platform === "win32") {
		return createErrorResult({
			kind: "unsupported",
			msg: "Directory synchronization is unavailable on Windows",
		});
	}
	return createSuccessResult(undefined);
}

/** The destination's metadata when it is a regular file with one link; undefined when it does not exist. */
async function inspectDestination(
	filePath: string,
): Promise<Result<BigIntStats | undefined, AtomicWriteError>> {
	const entry = await callWithResultAsync(() =>
		lstat(filePath, { bigint: true }),
	);
	if (entry.kind === "error") {
		return readErrorCode(entry.error) === "ENOENT"
			? createSuccessResult(undefined)
			: createErrorResult(
					toBeforePublicationError({ filePath, cause: entry.error }),
				);
	}
	// A concurrent replacement can unlink the inode while macOS reads its metadata.
	if (entry.value.isFile() && entry.value.nlink === 0n) {
		return createSuccessResult(undefined);
	}
	if (!entry.value.isFile() || entry.value.nlink !== 1n) {
		return createErrorResult({
			kind: "invalid-destination",
			msg: `${filePath} must be a regular file with one hard link; it is ${describeEntry(entry.value)}`,
		});
	}
	return createSuccessResult(entry.value);
}

function describeEntry(stats: BigIntStats): string {
	return stats.isSymbolicLink()
		? "a symbolic link"
		: stats.isDirectory()
			? "a directory"
			: stats.isFile()
				? `a regular file with ${stats.nlink} hard links`
				: "a special file";
}

function testPermissionsMatch(input: {
	readonly existing: BigIntStats;
	readonly permissions: AtomicWritePermissions;
}): boolean {
	return (
		input.permissions.kind === "platform-default" ||
		Number(input.existing.mode & 0o7777n) === input.permissions.mode
	);
}

/**
 * Checks whether the destination already contains the requested bytes and has
 * the requested mode. The comparison opens the file without following a link
 * and checks that the path still names the opened file after reading it.
 */
async function testDestinationUnchanged(input: {
	readonly filePath: string;
	readonly dir: string;
	readonly bytes: Uint8Array;
	readonly permissions: AtomicWritePermissions;
	readonly durability: AtomicWriteDurability;
	readonly existing: BigIntStats;
}): Promise<Result<boolean, AtomicWriteError>> {
	if (
		input.existing.size !== BigInt(input.bytes.length) ||
		!testPermissionsMatch(input)
	) {
		return createSuccessResult(false);
	}
	// Windows synchronizes a file only through a handle with write access.
	const access =
		process.platform === "win32" && input.durability !== "none"
			? constants.O_RDWR
			: constants.O_RDONLY;
	const opened = await callWithResultAsync(() =>
		open(input.filePath, access | NO_FOLLOW_FLAG),
	);
	if (opened.kind === "error") {
		return createErrorResult(
			toBeforePublicationError({
				filePath: input.filePath,
				cause: opened.error,
			}),
		);
	}
	const handle = opened.value;
	const compared = await callWithResultAsync(async () => {
		const contents = new Uint8Array(input.bytes.length + 1);
		let length = 0;
		for (;;) {
			const { bytesRead } = await handle.read(
				contents,
				length,
				contents.length - length,
				null,
			);
			if (bytesRead === 0) {
				break;
			}
			length += bytesRead;
			if (length === contents.length) {
				return false;
			}
		}
		if (
			length !== input.bytes.length ||
			!contents
				.subarray(0, length)
				.every((byte, index) => byte === input.bytes[index])
		) {
			return false;
		}
		const [opened, current] = await Promise.all([
			handle.stat({ bigint: true }),
			lstat(input.filePath, { bigint: true }),
		]);
		if (!testSameFile({ opened, current }) || current.nlink !== 1n) {
			return false;
		}
		if (input.durability !== "none") {
			await handle.sync();
		}
		return true;
	});
	const closed = await callWithResultAsync(() => handle.close());
	if (compared.kind === "error") {
		return createErrorResult(
			toBeforePublicationError({
				filePath: input.filePath,
				cause: compared.error,
			}),
		);
	}
	if (closed.kind === "error") {
		return createErrorResult(
			toBeforePublicationError({
				filePath: input.filePath,
				cause: closed.error,
			}),
		);
	}
	if (compared.value && input.durability === "file-and-dir") {
		const synchronized = await synchronizeDir(input.dir);
		if (synchronized.kind === "error") {
			return createErrorResult(
				toBeforePublicationError({
					filePath: input.filePath,
					cause: synchronized.error,
				}),
			);
		}
	}
	return createSuccessResult(compared.value);
}

async function publishFile(input: {
	readonly filePath: string;
	readonly dir: string;
	readonly bytes: Uint8Array;
	readonly permissions: AtomicWritePermissions;
	readonly durability: AtomicWriteDurability;
}): Promise<Result<AtomicWriteOutcome, AtomicWriteError>> {
	function toError(cause: unknown): AtomicWriteError {
		return toBeforePublicationError({ filePath: input.filePath, cause });
	}
	const mode = await resolvePublishedMode(input);
	if (mode.kind === "error") {
		return mode;
	}
	const temporary = await createTemporaryFile({
		dir: input.dir,
		mode: 0o600,
	});
	if (temporary.kind === "error") {
		return createErrorResult(toError(temporary.error));
	}
	const { handle, temporaryPath } = temporary.value;
	// Closing before the rename reports a write error that a file system
	// reports only on close, such as NFS, before the file is published.
	const written = await callWithResultAsync(async () => {
		await handle.writeFile(input.bytes);
		if (mode.value !== undefined) {
			await handle.chmod(mode.value);
		}
		if (input.durability !== "none") {
			await handle.sync();
		}
		return handle.stat({ bigint: true });
	});
	const closed = await callWithResultAsync(() => handle.close());
	const prepared =
		written.kind === "error"
			? createErrorResult(toError(written.error))
			: closed.kind === "error"
				? createErrorResult(toError(closed.error))
				: await checkPublicationPaths({
						written: written.value,
						temporaryPath,
						filePath: input.filePath,
					});
	const renamed =
		prepared.kind === "error"
			? prepared
			: mapResultError(
					await callWithResultAsync(() =>
						rename(temporaryPath, input.filePath),
					),
					toError,
				);
	if (renamed.kind === "error") {
		await callWithResultAsync(() => unlink(temporaryPath));
		return renamed;
	}
	if (input.durability === "file-and-dir") {
		const synchronized = await synchronizeDir(input.dir);
		if (synchronized.kind === "error") {
			return createErrorResult({
				kind: "published-but-not-durable",
				msg: `Published ${input.filePath}, but synchronizing ${input.dir} failed`,
				cause: synchronized.error,
			});
		}
	}
	return createSuccessResult({ kind: "published" });
}

/** The mode to apply before publication; undefined on Windows, where the platform decides. */
async function resolvePublishedMode(input: {
	readonly filePath: string;
	readonly dir: string;
	readonly permissions: AtomicWritePermissions;
}): Promise<Result<number | undefined, AtomicWriteError>> {
	if (input.permissions.kind === "unix") {
		return createSuccessResult(input.permissions.mode);
	}
	if (process.platform === "win32") {
		return createSuccessResult(undefined);
	}
	// An empty probe records the creation mode, including the umask and a
	// default ACL, without changing process-wide state.
	const probe = await createTemporaryFile({ dir: input.dir, mode: 0o666 });
	if (probe.kind === "error") {
		return createErrorResult(
			toBeforePublicationError({
				filePath: input.filePath,
				cause: probe.error,
			}),
		);
	}
	const stats = await callWithResultAsync(() => probe.value.handle.stat());
	await callWithResultAsync(() => probe.value.handle.close());
	await callWithResultAsync(() => unlink(probe.value.temporaryPath));
	return stats.kind === "error"
		? createErrorResult(
				toBeforePublicationError({
					filePath: input.filePath,
					cause: stats.error,
				}),
			)
		: createSuccessResult(stats.value.mode & 0o777);
}

const NO_FOLLOW_FLAG = process.platform === "win32" ? 0 : constants.O_NOFOLLOW;

async function createTemporaryFile(input: {
	readonly dir: string;
	readonly mode: number;
}): Promise<
	Result<
		{ readonly handle: FileHandle; readonly temporaryPath: string },
		unknown
	>
> {
	const random = new Uint8Array(16);
	const filled = fillRandomBytes(random);
	if (filled.kind === "error") {
		return filled;
	}
	const name = `.primitives-${encodeBytes(random, { encoding: "hex-lower" })}.tmp`;
	const temporaryPath = nodePath.join(input.dir, name);
	const opened = await callWithResultAsync(() => {
		return open(
			temporaryPath,
			constants.O_WRONLY |
				constants.O_CREAT |
				constants.O_EXCL |
				NO_FOLLOW_FLAG,
			input.mode,
		);
	});
	return opened.kind === "error"
		? opened
		: createSuccessResult({ handle: opened.value, temporaryPath });
}

/**
 * Checks, after writing, that the temporary path still names the written file
 * and that the destination is still absent or a regular file with one link.
 */
async function checkPublicationPaths(input: {
	readonly written: BigIntStats;
	readonly temporaryPath: string;
	readonly filePath: string;
}): Promise<Result<void, AtomicWriteError>> {
	const destination = await inspectDestination(input.filePath);
	if (destination.kind === "error") {
		return destination;
	}
	const current = await callWithResultAsync(() =>
		lstat(input.temporaryPath, { bigint: true }),
	);
	if (current.kind === "error") {
		return createErrorResult(
			toBeforePublicationError({
				filePath: input.filePath,
				cause: current.error,
			}),
		);
	}
	return testSameFile({ opened: input.written, current: current.value }) &&
		current.value.nlink === 1n
		? createSuccessResult(undefined)
		: createErrorResult({
				kind: "before-publication",
				msg: `Cannot write ${input.filePath}: the temporary file ${input.temporaryPath} was replaced while it was written`,
				cause: undefined,
			});
}

async function synchronizeDir(dir: string): Promise<Result<void, unknown>> {
	const opened = await callWithResultAsync(() =>
		open(dir, constants.O_RDONLY),
	);
	if (opened.kind === "error") {
		return opened;
	}
	const synchronized = await callWithResultAsync(() => opened.value.sync());
	const closed = await callWithResultAsync(() => opened.value.close());
	return synchronized.kind === "error" ? synchronized : closed;
}

function readErrorCode(error: unknown): string | undefined {
	return typeof error === "object" &&
		error !== null &&
		"code" in error &&
		typeof error.code === "string"
		? error.code
		: undefined;
}

function toBeforePublicationError(input: {
	readonly filePath: string;
	readonly cause: unknown;
}): AtomicWriteError {
	return {
		kind: "before-publication",
		msg: `Cannot write ${input.filePath}: ${input.cause instanceof Error ? input.cause.message : String(input.cause)}`,
		cause: input.cause,
	};
}

/** Windows volume serial numbers occupy the low 32 bits of libuv's device field. */
function testSameFile(input: {
	readonly opened: BigIntStats;
	readonly current: BigIntStats;
}): boolean {
	const openedDev =
		process.platform === "win32"
			? BigInt.asUintN(32, input.opened.dev)
			: input.opened.dev;
	const currentDev =
		process.platform === "win32"
			? BigInt.asUintN(32, input.current.dev)
			: input.current.dev;
	return openedDev === currentDev && input.opened.ino === input.current.ino;
}
