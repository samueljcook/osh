import { constants } from "node:fs";
import { lstat } from "node:fs/promises";
import nodePath from "node:path";
import { callWithResultAsync } from "../../../results/index.ts";
import {
	closeDescriptor,
	openDescriptor,
	readErrorCode,
	statDescriptor,
	toIoFailure,
	type OwnershipAttempt,
	type OwnershipFailure,
} from "./ownership.ts";

/*
On macOS, FreeBSD, and Windows, acquisition takes an exclusive OS lock on the
file `process.lock` in the lock directory. On macOS and FreeBSD, `open` takes a
BSD `flock` lock with `O_EXLOCK` and `O_NONBLOCK` and fails with `EAGAIN` when
another process owns the lock. On Windows, `open` passes `UV_FS_O_EXLOCK` so
that libuv calls `CreateFileW` with `dwShareMode = 0` and fails with `EBUSY`
when another process retains an open handle. The module documentation in
`./index.ts` explains why these platforms do not use the Linux socket protocol.
*/

/**
 * This constant equals `O_EXLOCK` in `<sys/fcntl.h>` on macOS and FreeBSD.
 * Node passes the flags of `fs.open` to `open(2)` unchanged, and it does not
 * export this constant.
 */
const BSD_O_EXLOCK = 0x20;

/**
 * This constant equals `UV_FS_O_EXLOCK` in libuv's `<uv/win.h>`. On Windows,
 * libuv translates this flag to `CreateFileW` with `dwShareMode = 0`.
 */
const WINDOWS_UV_FS_O_EXLOCK =
	(constants as { readonly UV_FS_O_EXLOCK?: number }).UV_FS_O_EXLOCK ??
	0x1000_0000;

export const LOCK_FILE_NAME = "process.lock";

export async function tryOwnThroughExclusiveOpen(input: {
	readonly dir: string;
}): Promise<OwnershipAttempt> {
	const lockPath = nodePath.join(input.dir, LOCK_FILE_NAME);
	const isWindows = process.platform === "win32";
	if (isWindows) {
		// Because Windows has no `O_NOFOLLOW` flag for `fs.open`, inspect any
		// existing entry before opening so that `CreateFileW` does not follow a
		// symbolic link or reparse point.
		const existing = await callWithResultAsync(() =>
			lstat(lockPath, { bigint: true }),
		);
		if (existing.kind === "ok") {
			if (
				existing.value.isSymbolicLink() ||
				!existing.value.isFile() ||
				existing.value.nlink !== 1n
			) {
				return {
					kind: "failed",
					error: createInvalidLockFileFailure(lockPath),
				};
			}
		} else if (readErrorCode(existing.error) !== "ENOENT") {
			return {
				kind: "failed",
				error: toIoFailure({
					msg: `Cannot inspect the lock file ${lockPath}`,
					cause: existing.error,
				}),
			};
		}
	}
	const opened = await openDescriptor({
		path: lockPath,
		flags: isWindows
			? constants.O_RDWR | constants.O_CREAT | WINDOWS_UV_FS_O_EXLOCK
			: constants.O_RDWR |
				constants.O_CREAT |
				constants.O_NOFOLLOW |
				constants.O_NONBLOCK |
				BSD_O_EXLOCK,
		mode: 0o666,
	});
	if (opened.kind === "error") {
		const code = readErrorCode(opened.error);
		const isContended = isWindows
			? code === "EBUSY"
			: code === "EAGAIN" || code === "EWOULDBLOCK";
		if (isContended) {
			return { kind: "contended" };
		}
		return {
			kind: "failed",
			error: await classifyLockOpenFailure({
				lockPath,
				cause: opened.error,
			}),
		};
	}
	const fd = opened.value;
	// Every participant must lock the same file, so the opened file must be the
	// only name of a regular file, and the path must still name it.
	const [openedStats, currentStats] = await Promise.all([
		statDescriptor(fd),
		callWithResultAsync(() => lstat(lockPath, { bigint: true })),
	]);
	const inspectionFailure =
		openedStats.kind === "error"
			? openedStats.error
			: currentStats.kind === "error"
				? currentStats.error
				: undefined;
	if (openedStats.kind === "error" || currentStats.kind === "error") {
		await closeDescriptor(fd);
		return {
			kind: "failed",
			error: toIoFailure({
				msg: `Cannot inspect the lock file ${lockPath}`,
				cause: inspectionFailure,
			}),
		};
	}
	const openedDev = isWindows
		? BigInt.asUintN(32, openedStats.value.dev)
		: openedStats.value.dev;
	const currentDev = isWindows
		? BigInt.asUintN(32, currentStats.value.dev)
		: currentStats.value.dev;
	if (
		!openedStats.value.isFile() ||
		!currentStats.value.isFile() ||
		currentStats.value.isSymbolicLink() ||
		openedStats.value.nlink !== 1n ||
		openedDev !== currentDev ||
		openedStats.value.ino !== currentStats.value.ino
	) {
		await closeDescriptor(fd);
		return {
			kind: "failed",
			error: createInvalidLockFileFailure(lockPath),
		};
	}
	return {
		kind: "owned",
		ownership: { dirPath: input.dir, end: () => closeDescriptor(fd) },
	};
}

async function classifyLockOpenFailure(input: {
	readonly lockPath: string;
	readonly cause: unknown;
}): Promise<OwnershipFailure> {
	const code = readErrorCode(input.cause);
	const entry = await callWithResultAsync(() => lstat(input.lockPath));
	if (
		entry.kind === "ok" &&
		(entry.value.isSymbolicLink() || !entry.value.isFile())
	) {
		return createInvalidLockFileFailure(input.lockPath);
	}
	if (
		(code === "EISDIR" ||
			code === "ELOOP" ||
			(process.platform === "freebsd" && code === "EMLINK")) &&
		entry.kind === "error" &&
		readErrorCode(entry.error) !== "ELOOP"
	) {
		return createInvalidLockFileFailure(input.lockPath);
	}
	return toIoFailure({
		msg: `Cannot open the lock file ${input.lockPath}`,
		cause: input.cause,
	});
}

function createInvalidLockFileFailure(lockPath: string): OwnershipFailure {
	return {
		kind: "invalid-lock-entry",
		path: lockPath,
		msg: `${lockPath} must be a regular file with one hard link that no process replaces`,
	};
}
