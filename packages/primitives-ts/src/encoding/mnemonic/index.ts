import { blake3 } from "@noble/hashes/blake3.js";
import { ENGLISH_WORDS } from "../../internal/english-words.ts";
import type { Result } from "../../results/index.ts";
import { createErrorResult, createSuccessResult } from "../../results/index.ts";
import type { ByteDecodeError } from "../binary-to-text/index.ts";

/** Failures contain no mnemonic words or decoded bytes. */
export type MnemonicDecodeError =
	| ByteDecodeError
	| {
			readonly kind:
				| "invalid-mnemonic-format"
				| "unsupported-version"
				| "nonzero-padding"
				| "checksum-mismatch";
	  }
	| { readonly kind: "unknown-word"; readonly wordIndex: number };

const WORD_INDICES = new Map(ENGLISH_WORDS.map((word, index) => [word, index]));
const CHECKSUM_DOMAIN = new TextEncoder().encode("mnemonic-codec/v1/checksum");

/**
 * Encodes arbitrary bytes in the Primitives mnemonic byte format. This is not
 * a BIP39 wallet mnemonic or seed derivation. Words use the BIP39 English list,
 * lowercase spelling, and single ASCII spaces. The checksum detects accidental
 * transcription errors; it does not authenticate data.
 *
 * The first word's index is 11 * version + padding, with version zero and 0–10
 * right zero bits. Data words encode MSB-first 11-bit groups. Two final words
 * encode the first 22 bits of BLAKE3(domain || u64be(byteLength) || bytes), with
 * domain "mnemonic-codec/v1/checksum". Empty input has three words.
 *
 * Applications importing this entry must declare `@noble/hashes` themselves.
 * The optional peer is required for this entry's BLAKE3 checksum. Other entries,
 * including random-word IDs, do not import the hashing dependency.
 */
export function encodeBytesAsMnemonic(bytes: Uint8Array): string {
	const padding = (11 - (((bytes.length % 11) * 8) % 11)) % 11;
	let text = ENGLISH_WORDS[padding]!;
	let accumulator = 0;
	let bits = 0;
	for (const byte of bytes) {
		accumulator = (accumulator << 8) | byte;
		bits += 8;
		if (bits >= 11) {
			bits -= 11;
			text += ` ${ENGLISH_WORDS[(accumulator >>> bits) & 2047]}`;
		}
	}
	if (bits > 0) {
		text += ` ${ENGLISH_WORDS[(accumulator << (11 - bits)) & 2047]}`;
	}
	const checksum = computeChecksum(bytes);
	return `${text} ${ENGLISH_WORDS[checksum >>> 11]} ${ENGLISH_WORDS[checksum & 2047]}`;
}

/**
 * Decodes canonical mnemonic text with an explicit decoded-byte allocation
 * limit. Applications must declare this entry's `@noble/hashes` peer.
 */
export function decodeMnemonicToBytes(
	text: string,
	options: { readonly maxDecodedSize: number },
): Result<Uint8Array, MnemonicDecodeError> {
	const parsed = parseMnemonicWords(text, options.maxDecodedSize);
	if (parsed.kind === "error") {
		return parsed;
	}
	const output = new Uint8Array(parsed.value.size);
	const result = decodeWordsInto(parsed.value, output);
	if (result.kind === "error") {
		output.fill(0);
		return result;
	}
	return createSuccessResult(output);
}

/**
 * Decodes into a caller-owned array and returns the written length. The entire
 * output is cleared on failure; bytes after the result are unchanged on success.
 * Applications must declare this entry's `@noble/hashes` peer.
 */
export function decodeMnemonicIntoBytes(
	text: string,
	options: { readonly output: Uint8Array },
): Result<number, MnemonicDecodeError> {
	const parsed = parseMnemonicWords(text, options.output.length);
	const result =
		parsed.kind === "error"
			? parsed
			: decodeWordsInto(parsed.value, options.output);
	if (result.kind === "error") {
		options.output.fill(0);
	}
	return result;
}

type ParsedMnemonic = {
	readonly words: ReadonlyArray<string>;
	readonly size: number;
};
function parseMnemonicWords(
	text: string,
	limit: number,
): Result<ParsedMnemonic, MnemonicDecodeError> {
	if (!Number.isSafeInteger(limit) || limit < 0) {
		return createErrorResult({ kind: "invalid-limit" });
	}
	if (text.length > (Math.ceil((limit * 8) / 11) + 3) * 9 - 1) {
		return createErrorResult({ kind: "output-too-small" });
	}
	const words = text.split(" ");
	if (words.length < 3 || words.some((word) => word.length === 0)) {
		return createErrorResult({ kind: "invalid-mnemonic-format" });
	}
	const header = WORD_INDICES.get(words[0]!);
	if (header === undefined) {
		return createErrorResult({ kind: "unknown-word", wordIndex: 0 });
	}
	if (header >= 11) {
		return createErrorResult({ kind: "unsupported-version" });
	}
	const bits = (words.length - 3) * 11 - header;
	if (bits < 0 || bits % 8 !== 0 || (bits === 0 && header !== 0)) {
		return createErrorResult({ kind: "invalid-mnemonic-format" });
	}
	if (bits / 8 > limit) {
		return createErrorResult({ kind: "output-too-small" });
	}
	return createSuccessResult({ words, size: bits / 8 });
}
function decodeWordsInto(
	parsed: ParsedMnemonic,
	output: Uint8Array,
): Result<number, MnemonicDecodeError> {
	let accumulator = 0;
	let bits = 0;
	let written = 0;
	let checksum = 0;
	for (let index = 1; index < parsed.words.length; index++) {
		const value = WORD_INDICES.get(parsed.words[index]!);
		if (value === undefined) {
			return createErrorResult({
				kind: "unknown-word",
				wordIndex: index,
			});
		}
		if (index >= parsed.words.length - 2) {
			checksum = (checksum << 11) | value;
			continue;
		}
		accumulator = (accumulator << 11) | value;
		bits += 11;
		while (bits >= 8 && written < parsed.size) {
			bits -= 8;
			output[written++] = accumulator >>> bits;
		}
		accumulator &= (1 << bits) - 1;
	}
	if (accumulator !== 0) {
		return createErrorResult({ kind: "nonzero-padding" });
	}
	if (computeChecksum(output.subarray(0, parsed.size)) !== checksum) {
		return createErrorResult({ kind: "checksum-mismatch" });
	}
	return createSuccessResult(parsed.size);
}
function computeChecksum(bytes: Uint8Array): number {
	const size = new Uint8Array(8);
	new DataView(size.buffer).setBigUint64(0, BigInt(bytes.length));
	const hasher = blake3.create();
	hasher.update(CHECKSUM_DOMAIN).update(size).update(bytes);
	const digest = hasher.digest();
	const checksum =
		((digest[0]! << 16) | (digest[1]! << 8) | digest[2]!) >>> 2;
	digest.fill(0);
	hasher.destroy();
	return checksum;
}
