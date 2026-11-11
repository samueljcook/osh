import type { Result } from "../../results/index.ts";
import { createErrorResult, createSuccessResult } from "../../results/index.ts";

/** Each encoding defines exactly one accepted spelling, including case and padding. */
export type ByteEncoding =
	| "hex-lower"
	| "hex-upper"
	| "base64"
	| "base64-unpadded"
	| "base64url"
	| "base64url-unpadded"
	| "base32"
	| "base32-unpadded"
	| "base32-lower-unpadded"
	| "crockford-base32"
	| "base58-bitcoin";

/** Errors contain positions and categories, never input or decoded content. */
export type ByteDecodeError =
	| {
			readonly kind:
				| "invalid-length"
				| "invalid-padding"
				| "output-too-small"
				| "invalid-limit";
	  }
	| {
			readonly kind: "invalid-symbol" | "nonzero-trailing-bits";
			readonly index: number;
	  };

const BASE58_ALPHABET =
	"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const FIXED_WIDTH_ENCODINGS = {
	"hex-lower": {
		alphabet: "0123456789abcdef",
		bitsPerSymbol: 4,
		paddingMultiple: 1,
	},
	"hex-upper": {
		alphabet: "0123456789ABCDEF",
		bitsPerSymbol: 4,
		paddingMultiple: 1,
	},
	base64: {
		alphabet:
			"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
		bitsPerSymbol: 6,
		paddingMultiple: 4,
	},
	"base64-unpadded": {
		alphabet:
			"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
		bitsPerSymbol: 6,
		paddingMultiple: 1,
	},
	base64url: {
		alphabet:
			"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_",
		bitsPerSymbol: 6,
		paddingMultiple: 4,
	},
	"base64url-unpadded": {
		alphabet:
			"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_",
		bitsPerSymbol: 6,
		paddingMultiple: 1,
	},
	base32: {
		alphabet: "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567",
		bitsPerSymbol: 5,
		paddingMultiple: 8,
	},
	"base32-unpadded": {
		alphabet: "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567",
		bitsPerSymbol: 5,
		paddingMultiple: 1,
	},
	"base32-lower-unpadded": {
		alphabet: "abcdefghijklmnopqrstuvwxyz234567",
		bitsPerSymbol: 5,
		paddingMultiple: 1,
	},
	"crockford-base32": {
		alphabet: "0123456789ABCDEFGHJKMNPQRSTVWXYZ",
		bitsPerSymbol: 5,
		paddingMultiple: 1,
	},
} as const;

/**
 * Encodes bytes without changing them. Crockford uses MSB-first byte groups and
 * right zero bits, without aliases, separators, or a check symbol. Base58 uses
 * Bitcoin's alphabet and preserves leading zeros; it has no Base58Check checksum.
 */
export function encodeBytes(
	bytes: Uint8Array,
	options: { readonly encoding: ByteEncoding },
): string {
	if (options.encoding === "base58-bitcoin") {
		return encodeBase58(bytes);
	}
	const { alphabet, bitsPerSymbol, paddingMultiple } =
		FIXED_WIDTH_ENCODINGS[options.encoding];
	let accumulator = 0;
	let availableBits = 0;
	let output = "";
	for (const byte of bytes) {
		accumulator = (accumulator << 8) | byte;
		availableBits += 8;
		while (availableBits >= bitsPerSymbol) {
			availableBits -= bitsPerSymbol;
			output +=
				alphabet[
					(accumulator >>> availableBits) & ((1 << bitsPerSymbol) - 1)
				];
		}
	}
	if (availableBits > 0) {
		output +=
			alphabet[
				(accumulator << (bitsPerSymbol - availableBits)) &
					((1 << bitsPerSymbol) - 1)
			];
	}
	return output.padEnd(
		Math.ceil(output.length / paddingMultiple) * paddingMultiple,
		"=",
	);
}

/**
 * Decodes canonical text with an explicit allocation limit in bytes. Whitespace,
 * prefixes, alternate case, misplaced padding, and nonzero unused bits fail.
 * Encoded strings and returned arrays are ordinary application-owned values.
 * `maxDecodedSize` must be a nonnegative safe integer. Base58 conversion takes
 * quadratic work, so choose a limit appropriate for identifiers or keys and use
 * Base64 for large payloads. Allocation failures remain runtime failures.
 */
export function decodeBytes(
	text: string,
	options: {
		readonly encoding: ByteEncoding;
		readonly maxDecodedSize: number;
	},
): Result<Uint8Array, ByteDecodeError> {
	const size = getDecodeBufferSize(text, options);
	if (size.kind === "error") {
		return size;
	}
	const output = new Uint8Array(size.value);
	const result = decodeBytesInto(text, {
		encoding: options.encoding,
		output,
	});
	if (result.kind === "error") {
		return result;
	}
	return createSuccessResult(output.subarray(0, result.value));
}

/**
 * Decodes into a caller-owned array and returns the written length. The entire
 * output is cleared on failure; bytes after the result are unchanged on success.
 * Input/output overlap cannot occur because the input is a JavaScript string.
 */
export function decodeBytesInto(
	text: string,
	options: { readonly encoding: ByteEncoding; readonly output: Uint8Array },
): Result<number, ByteDecodeError> {
	const result = decodeIntoBuffer(text, options);
	if (result.kind === "error") {
		options.output.fill(0);
	}
	return result;
}

function getDecodeBufferSize(
	text: string,
	options: {
		readonly encoding: ByteEncoding;
		readonly maxDecodedSize: number;
	},
): Result<number, ByteDecodeError> {
	const limit = options.maxDecodedSize;
	if (!Number.isSafeInteger(limit) || limit < 0) {
		return createErrorResult({ kind: "invalid-limit" });
	}
	if (options.encoding === "base58-bitcoin") {
		if (text.length > Math.floor(limit * 1.38) + 1) {
			return createErrorResult({ kind: "output-too-small" });
		}
		return createSuccessResult(Math.min(text.length, limit));
	}
	const { bitsPerSymbol, paddingMultiple } =
		FIXED_WIDTH_ENCODINGS[options.encoding];
	const firstPadding = text.indexOf("=");
	const symbolCount = firstPadding < 0 ? text.length : firstPadding;
	const padding = text.length - symbolCount;
	if (
		padding !==
		(paddingMultiple - (symbolCount % paddingMultiple)) % paddingMultiple
	) {
		return createErrorResult({ kind: "invalid-padding" });
	}
	for (let index = symbolCount; index < text.length; index++) {
		if (text[index] !== "=") {
			return createErrorResult({ kind: "invalid-padding" });
		}
	}
	const size = Math.floor((symbolCount * bitsPerSymbol) / 8);
	if (Math.ceil((size * 8) / bitsPerSymbol) !== symbolCount) {
		return createErrorResult({ kind: "invalid-length" });
	}
	if (size > limit) {
		return createErrorResult({ kind: "output-too-small" });
	}
	return createSuccessResult(size);
}

function decodeIntoBuffer(
	text: string,
	options: { readonly encoding: ByteEncoding; readonly output: Uint8Array },
): Result<number, ByteDecodeError> {
	const size = getDecodeBufferSize(text, {
		encoding: options.encoding,
		maxDecodedSize: options.output.length,
	});
	if (size.kind === "error") {
		return size;
	}
	if (options.encoding === "base58-bitcoin") {
		return decodeBase58Into(text, options.output);
	}
	const { alphabet, bitsPerSymbol } = FIXED_WIDTH_ENCODINGS[options.encoding];
	let accumulator = 0;
	let availableBits = 0;
	let written = 0;
	const symbolCount = Math.ceil((size.value * 8) / bitsPerSymbol);
	for (let index = 0; index < symbolCount; index++) {
		const value = alphabet.indexOf(text[index]!);
		if (value < 0) {
			return createErrorResult({ kind: "invalid-symbol", index });
		}
		accumulator = (accumulator << bitsPerSymbol) | value;
		availableBits += bitsPerSymbol;
		if (availableBits >= 8) {
			availableBits -= 8;
			options.output[written++] = accumulator >>> availableBits;
		}
	}
	if ((accumulator & ((1 << availableBits) - 1)) !== 0) {
		return createErrorResult({
			kind: "nonzero-trailing-bits",
			index: symbolCount - 1,
		});
	}
	return createSuccessResult(written);
}

function encodeBase58(bytes: Uint8Array): string {
	let leadingZeros = 0;
	while (leadingZeros < bytes.length && bytes[leadingZeros] === 0) {
		leadingZeros++;
	}
	const digits = new Uint8Array(
		Math.ceil((bytes.length - leadingZeros) * 1.38),
	);
	let used = 0;
	for (let index = leadingZeros; index < bytes.length; index++) {
		let carry = bytes[index]!;
		for (let digit = 0; digit < used; digit++) {
			carry += digits[digit]! * 256;
			digits[digit] = carry % 58;
			carry = Math.floor(carry / 58);
		}
		while (carry > 0) {
			digits[used++] = carry % 58;
			carry = Math.floor(carry / 58);
		}
	}
	let output = "1".repeat(leadingZeros);
	for (let index = used - 1; index >= 0; index--) {
		output += BASE58_ALPHABET[digits[index]!];
	}
	digits.fill(0);
	return output;
}

function decodeBase58Into(
	text: string,
	output: Uint8Array,
): Result<number, ByteDecodeError> {
	let leadingZeros = 0;
	while (leadingZeros < text.length && text[leadingZeros] === "1") {
		leadingZeros++;
	}
	if (leadingZeros > output.length) {
		return createErrorResult({ kind: "output-too-small" });
	}
	let used = 0;
	for (let index = leadingZeros; index < text.length; index++) {
		let carry = BASE58_ALPHABET.indexOf(text[index]!);
		if (carry < 0) {
			return createErrorResult({ kind: "invalid-symbol", index });
		}
		for (let byte = 0; byte < used; byte++) {
			carry += output[byte]! * 58;
			output[byte] = carry & 255;
			carry >>>= 8;
		}
		while (carry > 0) {
			if (used + leadingZeros === output.length) {
				return createErrorResult({ kind: "output-too-small" });
			}
			output[used++] = carry & 255;
			carry >>>= 8;
		}
	}
	output.subarray(0, used).reverse();
	output.copyWithin(leadingZeros, 0, used);
	output.fill(0, 0, leadingZeros);
	return createSuccessResult(used + leadingZeros);
}
