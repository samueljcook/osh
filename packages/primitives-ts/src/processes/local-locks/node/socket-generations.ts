import { constants, type Dirent } from "node:fs";
import { link, lstat, readdir, unlink } from "node:fs/promises";
import { connect, type Server } from "node:net";
import nodePath from "node:path";
import {
	callWithResultAsync,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../../../results/index.ts";
import {
	closeDescriptor,
	closeServer,
	createRandomId,
	listenExclusively,
	openDescriptor,
	readErrorCode,
	toIoFailure,
	type OwnershipAttempt,
	type OwnershipFailure,
} from "./ownership.ts";

/*
On Linux, acquisition uses listening Unix domain sockets in the lock directory,
which are named by generation numbers. The module documentation in `./index.ts`
states the protocol and the argument for its correctness.
*/

const CLAIM_NAME_PATTERN = /^claim-[0-9a-f]{32}\.sock$/;
const GENERATION_NAME_PATTERN = /^gen-([1-9][0-9]*)\.sock$/;
/** On Linux, `sun_path` has a capacity of 108 bytes, including the terminating null byte. */
const SOCKET_PATH_SIZE_LIMIT = 108;

type Generation = { readonly name: string; readonly number: bigint };

type LockDirEntries = {
	readonly generations: ReadonlyArray<Generation>;
	readonly claimNames: ReadonlyArray<string>;
};

/** Describes the state of the socket that a path names after a connection attempt. */
type SocketProbe =
	| { readonly kind: "listening" }
	| { readonly kind: "refused" }
	| { readonly kind: "missing" }
	| { readonly kind: "non-socket" }
	| { readonly kind: "failed"; readonly cause: unknown };

/** Represents the outcome of one pass of the protocol. */
type PassOutcome =
	| OwnershipAttempt
	/** Another participant changed the directory; a new pass reads it again. */
	| { readonly kind: "restart" };

/** Tracks a participant's claim socket after the participant linked the claim to its generation. */
type Announcement = {
	readonly server: Server;
	readonly generationPath: string;
};

export async function tryOwnThroughSocketGenerations(input: {
	readonly dir: string;
}): Promise<OwnershipAttempt> {
	const opened = await openDescriptor({
		path: input.dir,
		flags: constants.O_RDONLY | constants.O_DIRECTORY,
		mode: 0,
	});
	if (opened.kind === "error") {
		return failWith(
			toIoFailure({
				msg: `Cannot open the lock directory ${input.dir}`,
				cause: opened.error,
			}),
		);
	}
	const fd = opened.value;
	// Every path of this acquisition goes through the open descriptor, which
	// keeps socket addresses short and keeps the acquisition in one directory.
	const dirPath = nodePath.join("/proc/self/fd", String(fd));
	for (;;) {
		const outcome = await runPass({ dirPath, lockDir: input.dir, fd });
		if (outcome.kind === "restart") {
			continue;
		}
		if (outcome.kind !== "owned") {
			await closeDescriptor(fd);
		}
		return outcome;
	}
}

async function runPass(input: {
	readonly dirPath: string;
	readonly lockDir: string;
	readonly fd: number;
}): Promise<PassOutcome> {
	const { dirPath, lockDir } = input;
	const before = await listLockDir({ dirPath, lockDir });
	if (before.kind === "error") {
		return failWith(before.error);
	}
	const highest = findHighestGeneration(before.value.generations);
	if (highest !== undefined) {
		const probe = await probeSocket(nodePath.join(dirPath, highest.name));
		switch (probe.kind) {
			case "listening":
				return { kind: "contended" };
			case "missing":
				return { kind: "restart" };
			case "non-socket":
				return failWith(
					createInvalidSocketEntryFailure(
						nodePath.join(lockDir, highest.name),
					),
				);
			case "failed":
				return failWith(
					toIoFailure({
						msg: `Cannot connect to the lock socket ${highest.name}`,
						cause: probe.cause,
					}),
				);
			case "refused":
				break;
		}
	}
	const id = createRandomId();
	if (id.kind === "error") {
		return failWith(
			toIoFailure({ msg: "Cannot create a claim name", cause: id.error }),
		);
	}
	const claimName = `claim-${id.value}.sock`;
	const claimPath = nodePath.join(dirPath, claimName);
	const listening = await listenExclusively(claimPath);
	if (listening.kind === "error") {
		return failWith(
			toIoFailure({
				msg: `Cannot listen on the claim socket ${claimName}`,
				cause: listening.error,
			}),
		);
	}
	const server = listening.value;
	const generationName = `gen-${(highest?.number ?? 0n) + 1n}.sock`;
	const generationPath = nodePath.join(dirPath, generationName);
	const linked = await callWithResultAsync(() =>
		link(claimPath, generationPath),
	);
	if (linked.kind === "error") {
		await closeServer(server);
		const code = readErrorCode(linked.error);
		// When `link` fails with `EEXIST`, another participant linked this
		// generation first. When `link` fails with `ENOENT`, an owner removed the
		// claim while the claim refused connections between creation and listening.
		return code === "EEXIST" || code === "ENOENT"
			? { kind: "restart" }
			: failWith(
					toIoFailure({
						msg: `Cannot link the claim socket to ${generationName}`,
						cause: linked.error,
					}),
				);
	}
	return validateAnnouncement({
		dirPath,
		lockDir,
		fd: input.fd,
		claimName,
		generationName,
		announcement: { server, generationPath },
	});
}

/**
 * Decides the announcement's attempt from a listing that starts after the
 * announcement: a higher generation makes the attempt start again, and any
 * other listening generation makes it contended. Otherwise the participant
 * owns the lock and removes the files of participants that ended.
 */
async function validateAnnouncement(input: {
	readonly dirPath: string;
	readonly lockDir: string;
	readonly fd: number;
	readonly claimName: string;
	readonly generationName: string;
	readonly announcement: Announcement;
}): Promise<PassOutcome> {
	const { dirPath, lockDir, announcement } = input;
	const own = parseGeneration(input.generationName)!;
	const after = await listLockDir({ dirPath, lockDir });
	if (after.kind === "error") {
		await withdraw(announcement);
		return failWith(after.error);
	}
	const others = after.value.generations.filter((generation) => {
		return generation.name !== input.generationName;
	});
	if (others.some((generation) => generation.number > own.number)) {
		await withdraw(announcement);
		return { kind: "restart" };
	}
	const ended: Array<string> = [];
	for (const generation of others) {
		const probe = await probeSocket(
			nodePath.join(dirPath, generation.name),
		);
		if (probe.kind === "listening") {
			await withdraw(announcement);
			return { kind: "contended" };
		}
		if (probe.kind === "non-socket") {
			await withdraw(announcement);
			return failWith(
				createInvalidSocketEntryFailure(
					nodePath.join(lockDir, generation.name),
				),
			);
		}
		if (probe.kind === "failed") {
			await withdraw(announcement);
			return failWith(
				toIoFailure({
					msg: `Cannot connect to the lock socket ${generation.name}`,
					cause: probe.cause,
				}),
			);
		}
		if (probe.kind === "refused") {
			ended.push(generation.name);
		}
	}
	for (const claimName of after.value.claimNames) {
		if (claimName === input.claimName) {
			continue;
		}
		// A claim refuses connections after its participant ended, and also
		// between its creation and its listening; removing it in the second
		// case makes that participant's link fail and its attempt start again.
		const probe = await probeSocket(nodePath.join(dirPath, claimName));
		if (probe.kind === "non-socket") {
			await withdraw(announcement);
			return failWith(
				createInvalidSocketEntryFailure(
					nodePath.join(lockDir, claimName),
				),
			);
		}
		if (probe.kind === "failed") {
			await withdraw(announcement);
			return failWith(
				toIoFailure({
					msg: `Cannot connect to the claim socket ${claimName}`,
					cause: probe.cause,
				}),
			);
		}
		if (probe.kind === "refused") {
			ended.push(claimName);
		}
	}
	for (const name of ended) {
		const removed = await callWithResultAsync(() =>
			unlink(nodePath.join(dirPath, name)),
		);
		if (
			removed.kind === "error" &&
			readErrorCode(removed.error) !== "ENOENT"
		) {
			await withdraw(announcement);
			return failWith(
				toIoFailure({
					msg: `Cannot remove ${name} after its participant ended`,
					cause: removed.error,
				}),
			);
		}
	}
	return {
		kind: "owned",
		ownership: {
			dirPath,
			end: async () => {
				const removed = await callWithResultAsync(() =>
					unlink(announcement.generationPath),
				);
				// Close the descriptor after closing the server because Node removes
				// the claim through the descriptor's path when the server closes.
				const closed = await closeServer(announcement.server);
				const closedDir = await closeDescriptor(input.fd);
				return removed.kind === "error"
					? removed
					: closed.kind === "error"
						? closed
						: closedDir;
			},
		},
	};
}

/**
 * Removes the participant's generation file and then closes its socket so that
 * its generation never names a socket that refuses connections. Closing the
 * server in Node also removes the claim file.
 */
async function withdraw(announcement: Announcement): Promise<void> {
	// A generation file that stays after a failed removal refuses
	// connections once the socket closes, and the next owner removes it.
	await callWithResultAsync(() => unlink(announcement.generationPath));
	await closeServer(announcement.server);
}

async function listLockDir(input: {
	readonly dirPath: string;
	readonly lockDir: string;
}): Promise<Result<LockDirEntries, OwnershipFailure>> {
	const entries = await callWithResultAsync(() =>
		readdir(input.dirPath, { withFileTypes: true }),
	);
	if (entries.kind === "error") {
		return createErrorResult(
			toIoFailure({
				msg: "Cannot list the lock directory",
				cause: entries.error,
			}),
		);
	}
	const generations: Array<Generation> = [];
	const claimNames: Array<string> = [];
	for (const entry of entries.value) {
		const generation = parseGeneration(entry.name);
		const isClaim =
			generation === undefined && CLAIM_NAME_PATTERN.test(entry.name);
		if (generation === undefined && !isClaim) {
			continue;
		}
		if (!entry.isSocket()) {
			if (testDirentHasKnownNonSocketType(entry)) {
				return createErrorResult(
					createInvalidSocketEntryFailure(
						nodePath.join(input.lockDir, entry.name),
					),
				);
			}
			const inspected = await callWithResultAsync(() =>
				lstat(nodePath.join(input.dirPath, entry.name)),
			);
			if (inspected.kind === "error") {
				if (readErrorCode(inspected.error) === "ENOENT") {
					continue;
				}
				return createErrorResult(
					toIoFailure({
						msg: `Cannot inspect the lock entry ${entry.name}`,
						cause: inspected.error,
					}),
				);
			}
			if (!inspected.value.isSocket()) {
				return createErrorResult(
					createInvalidSocketEntryFailure(
						nodePath.join(input.lockDir, entry.name),
					),
				);
			}
		}
		if (generation !== undefined) {
			generations.push(generation);
		} else {
			claimNames.push(entry.name);
		}
	}
	return createSuccessResult({ generations, claimNames });
}

function testDirentHasKnownNonSocketType(entry: Dirent): boolean {
	return (
		entry.isFile() ||
		entry.isDirectory() ||
		entry.isSymbolicLink() ||
		entry.isFIFO() ||
		entry.isBlockDevice() ||
		entry.isCharacterDevice()
	);
}

function createInvalidSocketEntryFailure(entryPath: string): OwnershipFailure {
	return {
		kind: "invalid-lock-entry",
		path: entryPath,
		msg: `${entryPath} must be a Unix domain socket`,
	};
}

function parseGeneration(name: string): Generation | undefined {
	const digits = GENERATION_NAME_PATTERN.exec(name)?.[1];
	return digits === undefined ? undefined : { name, number: BigInt(digits) };
}

function findHighestGeneration(
	generations: ReadonlyArray<Generation>,
): Generation | undefined {
	let highest: Generation | undefined;
	for (const generation of generations) {
		if (highest === undefined || generation.number > highest.number) {
			highest = generation;
		}
	}
	return highest;
}

/**
 * Connects to a socket path and closes the connection at once. A connection
 * to a listening socket succeeds, or fails with EAGAIN when the listener's
 * queue is full, for example while its process is stopped. A connection that
 * the kernel queued fails with ECONNRESET when the listener closes before Node
 * reports it; the socket listened when the connection was made. ECONNREFUSED
 * means that no socket listens on the socket file, which is the case after its
 * listener closes.
 */
function probeSocket(path: string): Promise<SocketProbe> {
	if (Buffer.byteLength(path) >= SOCKET_PATH_SIZE_LIMIT) {
		return Promise.resolve({
			kind: "failed",
			cause: new Error(
				`The socket path ${path} is longer than the ${SOCKET_PATH_SIZE_LIMIT - 1}-byte limit`,
			),
		});
	}
	return new Promise((resolve) => {
		const socket = connect({ path });
		socket.once("connect", () => {
			socket.destroy();
			resolve({ kind: "listening" });
		});
		socket.once("error", (error) => {
			socket.destroy();
			switch (readErrorCode(error)) {
				case "EAGAIN":
				case "ECONNRESET":
					resolve({ kind: "listening" });
					return;
				case "ECONNREFUSED":
					resolve({ kind: "refused" });
					return;
				case "ENOENT":
					resolve({ kind: "missing" });
					return;
				case "ENOTSOCK":
				case "EISDIR":
					resolve({ kind: "non-socket" });
					return;
				default:
					resolve({ kind: "failed", cause: error });
			}
		});
	});
}

function failWith(error: OwnershipFailure): OwnershipAttempt {
	return { kind: "failed", error };
}
