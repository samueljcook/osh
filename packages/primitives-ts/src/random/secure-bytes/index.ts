import type { Result } from "../../results/index.ts";
import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
} from "../../results/index.ts";

/** A failure to obtain secure randomness from Web Crypto. */
export type RandomBytesError =
	| { readonly kind: "random-unavailable" }
	| { readonly kind: "random-failed"; readonly cause: unknown };

/**
 * Fills the entire array using Web Crypto in browsers or Node.js. Requests are
 * split at the platform's 65,536-byte limit. A failure clears the entire array,
 * including bytes from earlier successful requests. An empty array succeeds
 * without consulting the random source.
 */
export function fillRandomBytes(
	output: Uint8Array<ArrayBuffer>,
): Result<void, RandomBytesError> {
	if (output.length === 0) {
		return createSuccessResult(undefined);
	}
	const crypto = globalThis.crypto;
	if (crypto === undefined || typeof crypto.getRandomValues !== "function") {
		output.fill(0);
		return createErrorResult({ kind: "random-unavailable" });
	}
	// Web Crypto reports platform failures by throwing; this adapter returns them.
	const filled = callWithResult(() => {
		for (let offset = 0; offset < output.length; offset += 65_536) {
			crypto.getRandomValues(output.subarray(offset, offset + 65_536));
		}
	});
	if (filled.kind === "error") {
		output.fill(0);
		return createErrorResult({
			kind: "random-failed",
			cause: filled.error,
		});
	}
	return createSuccessResult(undefined);
}
