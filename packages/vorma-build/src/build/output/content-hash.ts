import { blake3 } from "@noble/hashes/blake3.js";
import { utf8ToBytes } from "@noble/hashes/utils.js";
import { Effect, Result, Stream, type Schema } from "effect";
import * as FileSystem from "effect/FileSystem";
import { encodeBytes } from "primitives-ts/encoding/binary-to-text";
import { stringifyJsonStable } from "primitives-ts/serialization/json";

/** Vorma could not serialize a client build's identity. */
export type ClientBuildIdIssue = {
	readonly _tag: "ClientBuildIdIssue";
	readonly msg: string;
};

/** BLAKE3 content identity, encoded as lowercase base32 without padding. */
export function toContentHash(source: string | Uint8Array): string {
	return encodeContentHash(
		blake3(typeof source === "string" ? utf8ToBytes(source) : source),
	);
}

/** Encodes an already computed BLAKE3 hash. */
export function encodeContentHash(hash: Uint8Array): string {
	return encodeBytes(hash, { encoding: "base32-lower-unpadded" });
}

/** Computes a file's BLAKE3 content identity with bounded streaming reads. */
export function hashFile(filePath: string) {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const hasher = blake3.create();
		yield* Stream.runForEach(fileSystem.stream(filePath), (chunk) => {
			return Effect.sync(() => {
				hasher.update(chunk);
			});
		});
		return hasher.digest();
	});
}

/** The leading 24 characters of the content hash of a client build's canonical identity. */
export function toClientBuildId(
	identity: Schema.Json,
): Result.Result<string, ClientBuildIdIssue> {
	const serialized = stringifyJsonStable(identity, {
		format: { kind: "compact" },
	});
	if (serialized.kind === "error") {
		return Result.fail({
			_tag: "ClientBuildIdIssue",
			msg: `Vorma could not serialize the client build identity (${serialized.error.kind})`,
		});
	}
	return Result.succeed(toContentHash(serialized.value).slice(0, 24));
}
