import { close, fstat, open } from "node:fs";
import type { BigIntStats } from "node:fs";
import { createServer, type Server } from "node:net";
import { encodeBytes } from "../../../encoding/binary-to-text/index.ts";
import { fillRandomBytes } from "../../../random/secure-bytes/index.ts";
import {
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../../../results/index.ts";

/*
Defines the result types that each platform variant returns to the public entry
and the Node descriptor and socket helpers that the variants share. Variants
use raw file descriptors instead of `FileHandle` objects because Node closes an
unreferenced `FileHandle` during garbage collection, which would end ownership
at an unpredictable time.
*/

/** Represents the outcome of one acquisition attempt by a platform variant. */
export type OwnershipAttempt =
	| { readonly kind: "owned"; readonly ownership: PlatformOwnership }
	| { readonly kind: "contended" }
	| { readonly kind: "failed"; readonly error: OwnershipFailure };

export type OwnershipFailure =
	| {
			readonly kind: "invalid-lock-entry";
			readonly path: string;
			readonly msg: string;
	  }
	| { readonly kind: "io"; readonly msg: string; readonly cause: unknown };

/** Represents the OS resource that a platform variant keeps open for an owner. */
export type PlatformOwnership = {
	/**
	 * Names the path through which this owner accesses the lock directory. On
	 * Linux, this path names the owner's open file descriptor for the directory.
	 */
	readonly dirPath: string;
	/**
	 * Ends ownership. A variant removes what it announced in the directory and
	 * then closes the OS resource; the resource is closed even when a removal
	 * fails, and the result then reports the removal failure.
	 */
	readonly end: () => Promise<Result<void, unknown>>;
};

export function readErrorCode(error: unknown): string | undefined {
	return typeof error === "object" &&
		error !== null &&
		"code" in error &&
		typeof error.code === "string"
		? error.code
		: undefined;
}

export function describeError(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

export function toIoFailure(input: {
	readonly msg: string;
	readonly cause: unknown;
}): OwnershipFailure {
	return {
		kind: "io",
		msg: `${input.msg}: ${describeError(input.cause)}`,
		cause: input.cause,
	};
}

/** Generates a lowercase hexadecimal identifier from 16 random bytes. */
export function createRandomId(): Result<string, unknown> {
	const bytes = new Uint8Array(16);
	const filled = fillRandomBytes(bytes);
	return filled.kind === "error"
		? filled
		: createSuccessResult(encodeBytes(bytes, { encoding: "hex-lower" }));
}

export function openDescriptor(input: {
	readonly path: string;
	readonly flags: number;
	readonly mode: number;
}): Promise<Result<number, unknown>> {
	return new Promise((resolve) => {
		open(input.path, input.flags, input.mode, (error, fd) => {
			resolve(
				error === null
					? createSuccessResult(fd)
					: createErrorResult(error),
			);
		});
	});
}

export function closeDescriptor(fd: number): Promise<Result<void, unknown>> {
	return new Promise((resolve) => {
		close(fd, (error) => {
			resolve(
				error === null
					? createSuccessResult(undefined)
					: createErrorResult(error),
			);
		});
	});
}

export function statDescriptor(
	fd: number,
): Promise<Result<BigIntStats, unknown>> {
	return new Promise((resolve) => {
		fstat(fd, { bigint: true }, (error, stats) => {
			resolve(
				error === null
					? createSuccessResult(stats)
					: createErrorResult(error),
			);
		});
	});
}

/**
 * Listens on a Unix domain socket path. The server closes every connection
 * that it accepts at once and does not keep the Node process alive. A failure
 * to accept a connection does not close the listener, so the server ignores it.
 */
export function listenExclusively(
	path: string,
): Promise<Result<Server, unknown>> {
	return new Promise((resolve) => {
		const server = createServer((socket) => socket.destroy());
		function onListenError(error: Error) {
			resolve(createErrorResult(error));
		}
		server.once("error", onListenError);
		server.listen(path, () => {
			server.off("error", onListenError);
			server.on("error", () => {});
			server.unref();
			resolve(createSuccessResult(server));
		});
	});
}

/** Closes a listener; on Unix, Node also removes the path that the listener bound. */
export function closeServer(server: Server): Promise<Result<void, unknown>> {
	return new Promise((resolve) => {
		server.close((error) => {
			resolve(
				error === undefined
					? createSuccessResult(undefined)
					: createErrorResult(error),
			);
		});
	});
}
