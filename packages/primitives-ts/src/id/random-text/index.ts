import {
	fillRandomBytes,
	type RandomBytesError,
} from "../../random/secure-bytes/index.ts";
import type { Result } from "../../results/index.ts";
import { createErrorResult, createSuccessResult } from "../../results/index.ts";
import { generateRandomTextIdWithSource } from "./generation.ts";

declare const RANDOM_ID_ALPHABET: unique symbol;

/** An alphabet of 2–128 distinct ASCII characters, produced by parsing or a named constant. */
export type RandomTextIdAlphabet = string & {
	readonly [RANDOM_ID_ALPHABET]: true;
};

/** Explicit alphabet choices; custom alphabets use parseRandomTextIdAlphabet. */
export const RANDOM_TEXT_ID_ALPHABETS = {
	lowercaseHex: "0123456789abcdef" as RandomTextIdAlphabet,
	lowercaseAlphanumeric:
		"0123456789abcdefghijklmnopqrstuvwxyz" as RandomTextIdAlphabet,
	mixedCaseAlphanumeric:
		"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz" as RandomTextIdAlphabet,
	base64Url:
		"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_" as RandomTextIdAlphabet,
} as const;

/** A malformed random-ID alphabet. */
export type RandomTextIdAlphabetError =
	| { readonly kind: "invalid-alphabet-length" }
	| {
			readonly kind:
				| "non-ascii-alphabet"
				| "duplicate-alphabet-character";
			readonly index: number;
	  };

/** A malformed identifier length or alphabet, or a secure-randomness failure. */
export type RandomTextIdError =
	| { readonly kind: "invalid-length" }
	| RandomTextIdAlphabetError
	| RandomBytesError;

/**
 * Parses distinct ASCII characters without normalization or case conversion.
 * Explicit whitespace and control characters are allowed. Duplicate characters
 * are rejected so that each character has the same sampling probability.
 */
export function parseRandomTextIdAlphabet(
	symbols: string,
): Result<RandomTextIdAlphabet, RandomTextIdAlphabetError> {
	if (symbols.length < 2 || symbols.length > 128) {
		return createErrorResult({ kind: "invalid-alphabet-length" });
	}
	const seen = new Set<number>();
	for (let index = 0; index < symbols.length; index += 1) {
		const code = symbols.charCodeAt(index);
		if (code > 127) {
			return createErrorResult({ kind: "non-ascii-alphabet", index });
		}
		if (seen.has(code)) {
			return createErrorResult({
				kind: "duplicate-alphabet-character",
				index,
			});
		}
		seen.add(code);
	}
	return createSuccessResult(symbols as RandomTextIdAlphabet);
}

/** Explicit alphabet and character count for a random text identifier. */
export type RandomTextIdOptions = {
	readonly alphabet: RandomTextIdAlphabet;
	/** A positive safe integer. ASCII character and byte counts are equal. */
	readonly length: number;
};

/**
 * Generates exactly length characters with unbiased secure sampling. The result
 * has length * log2(alphabet.length) bits of entropy and contains no timestamp.
 * It is ordinary public text; applications with persistent ID namespaces must
 * handle collisions. Memory exhaustion remains a runtime allocation failure.
 * This function parses the alphabet again, so an invalid alphabet, such as a
 * string that a cast typed as RandomTextIdAlphabet, returns its alphabet error
 * before any sampling.
 */
export function generateRandomTextId(
	options: RandomTextIdOptions,
): Result<string, RandomTextIdError> {
	const alphabet = parseRandomTextIdAlphabet(options.alphabet);
	if (alphabet.kind === "error") {
		return alphabet;
	}
	return generateRandomTextIdWithSource(options, fillRandomBytes);
}
