import { stat } from "node:fs/promises";
import {
	callWithResultAsync,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../../../results/index.ts";
import { tryOwnThroughExclusiveOpen } from "./exclusive-open.ts";
import {
	OWNER_RECORD_SIZE_LIMIT,
	readOwnerRecord,
	removeOwnerRecord,
	writeOwnerRecord,
} from "./owner-records.ts";
import {
	describeError,
	type OwnershipAttempt,
	type PlatformOwnership,
} from "./ownership.ts";
import { tryOwnThroughSocketGenerations } from "./socket-generations.ts";

/*
# Local process locks

A process lock gives at most one process at a time ownership of a dedicated
lock directory. It coordinates processes that run on one host, including
containers on that host that reach the directory through a mount; a process
in a virtual machine runs on another host. Ownership is determined by an
operating-system resource that the kernel releases when the owner's process
ends for any reason, including SIGKILL. Ownership does not depend on any
process ID, clock, or time limit. The owner record that an application
supplies is diagnostic text, and an application must never use that record to
determine whether a process owns the lock.

Acquisition never waits: it returns `contended` at once when another process
owns the lock, and the application decides whether to try again. A stopped or
suspended owner keeps the lock. A process that ends at any point leaves
nothing that a person must remove: the next owner removes what the ended
process left.

Every participant must use the same directory, and the directory must stay in
place while any participant can use it: an application must not rename,
replace, or remove it. The directory must exist before acquisition, must be
dedicated to the lock, and must not be writable by an untrusted user.
Acquisition ignores entries with names that the protocol does not use. Every
participant needs permission to read, write, and search the directory. The
lock's OS resource must stay in the owner's process: Node does not pass it to
child processes, and an application must not pass it explicitly. After
acquiring the lock, the owner writes its owner record to `owner.record` in the
directory through a temporary file `record-<id>.tmp` and one rename; it
removes the record when releasing the lock, so a record remains after an owner
only when that owner ended without releasing the lock.

Linux uses listening Unix domain sockets, macOS and FreeBSD use an exclusive
`flock` lock that `open` takes with `O_EXLOCK`, and Windows opens
`process.lock` with `UV_FS_O_EXLOCK` so that `CreateFileW` uses exclusive file
sharing (`dwShareMode = 0`). The macOS, FreeBSD, and Windows variants have not
been run. On other platforms, each operation returns `unsupported-platform`
without reading or modifying the file system. TypeScript applications must
declare `@types/node`.

## Linux

A listening Unix domain socket in the lock directory indicates a live
participant. The kernel closes the listener when its process ends. After the
listener closes, a connection to its socket file fails with ECONNREFUSED, and
that socket file can never listen again.
A connection to a listening socket succeeds, or fails with EAGAIN when the
listener's queue is full, which happens while the owner is stopped. A
connection that the kernel queued fails with ECONNRESET when the listener
closes before Node reports the connection. A participant treats both errors as
evidence of a live listener, so an attempt returns `contended` or starts
again. On Linux, each participant needs write permission on the socket files
of other participants, and the process umask controls the permissions of newly
created socket files. A participant reaches the directory through
`/proc/self/fd/<descriptor>` of a descriptor that it keeps open for the whole
acquisition and ownership. That path keeps every socket address below the
108-byte limit, and it keeps every operation of one participant in one
directory.

Participants create two kinds of socket files in the directory. Each
acquisition attempt creates a `claim-<id>.sock` socket file with a random
128-bit `id`. After that socket listens, the attempt links it to
`gen-<n>.sock`, where `n` is a positive decimal generation number. A
generation file represents a participant that can become the owner.

An attempt lists the directory and takes the highest generation `n`, or 0 when
there is none. When `gen-<n>.sock` listens, the attempt returns `contended`.
When it refuses connections, the attempt listens on a new claim socket and
links the claim to `gen-<n+1>.sock`. When that name already exists, another
participant claimed the generation first, so the attempt closes its socket and
starts again. The attempt then lists the directory again. When the listing
includes a generation above its own, the attempt withdraws and starts again.
When any other generation file listens, the attempt withdraws and returns
`contended`. Otherwise the participant owns the lock, and it removes the
generation files and claim files that refused its connections. To withdraw,
and to release the lock, a participant removes its generation file and then
closes its socket, which also removes its claim file.

The argument for correctness uses four facts. First, a generation file names
a listening socket from its creation until its participant removes the file;
a participant that ends without removing it leaves a file that refuses
connections forever. Second, a participant removes another participant's file
only as the owner, and only after a connection to the file was refused. While
such a file exists, no other participant can create a file with its name, and
only one owner removes files at a time, so the file that the owner removes is
the file that refused the connection. A generation file that listens is
therefore never removed by another participant. A claim file does not
establish a generation: the socket refuses connections between creation and
listening, and when the owner removes such a claim, that attempt's `link` call
fails so that the attempt starts again. Third, a directory listing returns
every entry that exists from the start of the listing to its end: POSIX leaves
unspecified only whether a listing returns an entry that is created or
removed during the listing. Fourth, on Linux a connection to a listening
socket never fails with ECONNREFUSED.

Suppose that two participants P and Q owned the lock at the same time, and
that P created its generation file before Q created its own. Q's second
listing started after Q created its file and ended before Q became the owner.
P's generation file existed during that whole listing: P created it earlier,
P does not remove it while it validates or owns the lock, and by the second
fact no other participant removes it. By the third fact, Q's listing included
P's file. Q then either found P's generation above its own and withdrew, or
connected to P's socket, which listened, and withdrew. Neither case lets Q
own the lock, so at most one participant owns the lock at any time. The
argument does not use the generation numbers or the order of events in
time beyond the creation order of two files. Because attempts that read the
same highest generation compete to create the same next generation name, one
of those attempts becomes the owner instead of none.

A participant that ends at any point leaves at most one claim file and one
generation file, which refuse connections. The next owner removes them. A
stopped owner keeps its socket, so other attempts return `contended`. An
attempt also returns `contended` when it encounters the generation file of
another attempt that will itself withdraw; no participant then owns the lock,
and a later attempt can acquire it. An attempt starts again only after another
participant changed the directory, by creating or removing a generation file
or by removing the attempt's claim.

## macOS and FreeBSD

On macOS and FreeBSD, a connection to a listening socket with a full queue
fails with ECONNREFUSED, the same error as a connection after the socket's
listener closes. A queue fills when the owner is stopped or its event loop is
blocked while other processes try to acquire the lock, so the Linux protocol
would let a second process take the lock from a live owner. These platforms
instead open the file `process.lock` in the lock directory with `O_EXLOCK` and
`O_NONBLOCK`, which takes an exclusive `flock` lock or fails at once with
EAGAIN. The kernel releases the lock when the descriptor closes, which
happens when the process ends for any reason. Node opens every file with
`O_CLOEXEC`, so a child process does not keep the lock. The lock never
removes or replaces `process.lock`, so every participant locks one file; it
refuses a symbolic link, a file with more than one hard link, and a path that
names another file after the open.

## Windows

On Windows, a participant opens `process.lock` in the lock directory with
`UV_FS_O_EXLOCK`, which libuv passes to `CreateFileW` as `dwShareMode = 0`. A
second open while the handle remains open fails at once with `EBUSY`. The NT
kernel closes the handle and releases the sharing lock when the owner's process
ends for any reason, and a suspended owner retains its open handle. Before and
after the open, the participant inspects `process.lock` with `lstat` and
`fstat` so that a symbolic link, reparse point, directory, or file with more
than one hard link fails with `invalid-lock-entry`.
*/

/** Configures one lock acquisition attempt. */
export type ProcessLockOptions = {
	/** Names the dedicated lock directory, which must exist before acquisition. */
	readonly dir: string;
	/**
	 * Specifies diagnostic text, at most 4096 bytes in UTF-8, that the owner
	 * writes in the lock directory after acquiring the lock, such as a JSON
	 * object with its process ID. Contending participants receive this text as
	 * `recordedOwnerRecord`.
	 */
	readonly ownerRecord: string;
};

/** Describes why a lock acquisition attempt failed. */
export type ProcessLockError =
	| {
			/** Another process owned the lock when acquisition tried to take it. */
			readonly kind: "contended";
			/**
			 * Contains the owner record from the lock directory when acquisition read
			 * it, or undefined when the directory contained no readable record. This
			 * value can be the record of an earlier owner that ended without
			 * releasing the lock while the current owner has not written its own
			 * record yet. An application must never use this value to authorize lock
			 * recovery or process termination.
			 */
			readonly recordedOwnerRecord: string | undefined;
	  }
	| { readonly kind: "invalid-options"; readonly msg: string }
	| { readonly kind: "unsupported-platform"; readonly msg: string }
	/** An entry that the lock uses has a type, link count, or identity that the protocol never creates. */
	| {
			readonly kind: "invalid-lock-entry";
			readonly path: string;
			readonly msg: string;
	  }
	| { readonly kind: "io"; readonly msg: string; readonly cause: unknown };

/** Describes a cleanup failure during lock release; release closes the OS resource even when cleanup fails. */
export type ProcessLockReleaseError = {
	readonly kind: "io";
	readonly msg: string;
	readonly cause: unknown;
};

/** Represents an acquired process lock. */
export type ProcessLock = {
	/**
	 * Ends ownership by removing the owner record and any directory entries
	 * that this owner created, and then closing the OS resource. Release closes
	 * the OS resource even when a removal fails, and the returned result
	 * reports that failure. A second call returns the result of the first call.
	 * If the application does not release the lock, the owner retains it until
	 * the process ends.
	 */
	readonly release: () => Promise<Result<void, ProcessLockReleaseError>>;
};

const SUPPORTED_PLATFORMS: ReadonlySet<NodeJS.Platform> = new Set([
	"linux",
	"darwin",
	"freebsd",
	"win32",
]);

/**
 * Tries once to acquire the lock of `dir`, as the module documentation
 * describes. On success, the owner has written its record.
 */
export async function tryAcquireProcessLock(
	options: ProcessLockOptions,
): Promise<Result<ProcessLock, ProcessLockError>> {
	if (!SUPPORTED_PLATFORMS.has(process.platform)) {
		return createErrorResult({
			kind: "unsupported-platform",
			msg: `Process locks are unsupported on ${process.platform}`,
		});
	}
	if (typeof options.dir !== "string" || options.dir === "") {
		return createErrorResult({
			kind: "invalid-options",
			msg: "The lock directory must be a nonempty path",
		});
	}
	if (
		typeof options.ownerRecord !== "string" ||
		new TextEncoder().encode(options.ownerRecord).length >
			OWNER_RECORD_SIZE_LIMIT
	) {
		return createErrorResult({
			kind: "invalid-options",
			msg: `The owner record must be a string of at most ${OWNER_RECORD_SIZE_LIMIT} bytes in UTF-8`,
		});
	}
	const entry = await callWithResultAsync(() => stat(options.dir));
	if (entry.kind === "error" || !entry.value.isDirectory()) {
		return createErrorResult({
			kind: "io",
			msg: `The lock directory ${options.dir} must be an existing directory${entry.kind === "error" ? `: ${describeError(entry.error)}` : ""}`,
			cause: entry.kind === "error" ? entry.error : undefined,
		});
	}
	const attempt = await tryOwnOnPlatform(options.dir);
	switch (attempt.kind) {
		case "contended": {
			const record = await readOwnerRecord(options.dir);
			return createErrorResult({
				kind: "contended",
				recordedOwnerRecord:
					record.kind === "ok" ? record.value : undefined,
			});
		}
		case "failed":
			return createErrorResult(attempt.error);
		case "owned":
			return completeOwnership({
				ownership: attempt.ownership,
				ownerRecord: options.ownerRecord,
			});
	}
}

function tryOwnOnPlatform(dir: string): Promise<OwnershipAttempt> {
	return process.platform === "linux"
		? tryOwnThroughSocketGenerations({ dir })
		: tryOwnThroughExclusiveOpen({ dir });
}

async function completeOwnership(input: {
	readonly ownership: PlatformOwnership;
	readonly ownerRecord: string;
}): Promise<Result<ProcessLock, ProcessLockError>> {
	const written = await writeOwnerRecord({
		dirPath: input.ownership.dirPath,
		ownerRecord: input.ownerRecord,
	});
	if (written.kind === "error") {
		await input.ownership.end();
		return createErrorResult({
			kind: "io",
			msg: `Cannot write the owner record: ${describeError(written.error)}`,
			cause: written.error,
		});
	}
	type LockState =
		| { readonly kind: "held"; readonly ownership: PlatformOwnership }
		| {
				readonly kind: "released";
				readonly result: Promise<Result<void, ProcessLockReleaseError>>;
		  };
	let state: LockState = { kind: "held", ownership: input.ownership };
	return createSuccessResult({
		release: () => {
			if (state.kind === "released") {
				return state.result;
			}
			const result = releaseOwnership(state.ownership);
			state = { kind: "released", result };
			return result;
		},
	});
}

async function releaseOwnership(
	ownership: PlatformOwnership,
): Promise<Result<void, ProcessLockReleaseError>> {
	const removed = await removeOwnerRecord(ownership.dirPath);
	const ended = await ownership.end();
	const failure =
		removed.kind === "error"
			? removed.error
			: ended.kind === "error"
				? ended.error
				: undefined;
	return failure === undefined
		? createSuccessResult(undefined)
		: createErrorResult({
				kind: "io",
				msg: `Releasing the process lock failed: ${describeError(failure)}`,
				cause: failure,
			});
}

/** Describes why `readProcessLockOwnerRecord` could not return a lock directory's owner record. */
export type ProcessLockRecordError =
	| { readonly kind: "invalid-options"; readonly msg: string }
	| { readonly kind: "unsupported-platform"; readonly msg: string }
	| { readonly kind: "invalid-record"; readonly msg: string }
	| { readonly kind: "io"; readonly msg: string; readonly cause: unknown };

/**
 * Reads the owner record of a lock directory without acquiring the lock. The
 * returned value is the current owner's record, the record of an earlier owner
 * that ended without releasing the lock, or undefined when the directory
 * contains no record. This value is diagnostic text, as `ProcessLockError`
 * describes for `contended`.
 */
export async function readProcessLockOwnerRecord(input: {
	readonly dir: string;
}): Promise<Result<string | undefined, ProcessLockRecordError>> {
	if (typeof input.dir !== "string" || input.dir === "") {
		return createErrorResult({
			kind: "invalid-options",
			msg: "The lock directory must be a nonempty path",
		});
	}
	if (!SUPPORTED_PLATFORMS.has(process.platform)) {
		return createErrorResult({
			kind: "unsupported-platform",
			msg: `Process locks are unsupported on ${process.platform}`,
		});
	}
	const record = await readOwnerRecord(input.dir);
	if (record.kind === "ok") {
		return record;
	}
	switch (record.error.kind) {
		case "oversized":
			return createErrorResult({
				kind: "invalid-record",
				msg: `The owner record in ${input.dir} is longer than ${OWNER_RECORD_SIZE_LIMIT} bytes`,
			});
		case "invalid-utf8":
			return createErrorResult({
				kind: "invalid-record",
				msg: `The owner record in ${input.dir} is not UTF-8 text`,
			});
		case "io":
			return createErrorResult({
				kind: "io",
				msg: `Cannot read the owner record in ${input.dir}: ${describeError(record.error.cause)}`,
				cause: record.error.cause,
			});
	}
}
