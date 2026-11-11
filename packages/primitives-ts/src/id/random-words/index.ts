import type { RandomBytesError } from "../../random/secure-bytes/index.ts";
import { fillRandomBytes } from "../../random/secure-bytes/index.ts";
import type { Result } from "../../results/index.ts";
import { generateRandomWordIdWithSource } from "./generation.ts";

/** A malformed word count or a secure-randomness failure. */
export type RandomWordIdError =
	| { readonly kind: "invalid-length" }
	| RandomBytesError;

/** Explicit word count for a hyphen-separated English identifier. */
export type RandomWordIdOptions = {
	/** A positive safe integer; the maximum possible text size must also be a safe integer. */
	readonly wordCount: number;
};

/**
 * Generates lowercase English words separated by single hyphens. Each word is
 * sampled independently from the fixed 2,048-word BIP39 English vocabulary,
 * giving 11 bits of entropy per word. Words may repeat. Three words provide
 * 33 bits; twelve provide 132 bits. Persistent namespaces must handle collisions.
 *
 * The result is a public identifier with no encoded payload or checksum, not
 * the mnemonic byte format. This entry imports no hashing dependency. Memory
 * exhaustion remains a runtime allocation failure.
 */
export function generateRandomWordId(
	options: RandomWordIdOptions,
): Result<string, RandomWordIdError> {
	return generateRandomWordIdWithSource(options, fillRandomBytes);
}
