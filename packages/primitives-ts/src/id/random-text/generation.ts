import type { RandomBytesError } from "../../random/secure-bytes/index.ts";
import type { Result } from "../../results/index.ts";
import { createErrorResult, createSuccessResult } from "../../results/index.ts";
import type { RandomTextIdOptions, RandomTextIdError } from "./index.ts";
export function generateRandomTextIdWithSource(
	options: RandomTextIdOptions,
	randomFillFn: (
		output: Uint8Array<ArrayBuffer>,
	) => Result<void, RandomBytesError>,
): Result<string, RandomTextIdError> {
	if (!Number.isSafeInteger(options.length) || options.length <= 0) {
		return createErrorResult({ kind: "invalid-length" });
	}
	const output = new Uint8Array(options.length);
	const random = new Uint8Array(Math.min(options.length, 256));
	const acceptedValues =
		Math.floor(256 / options.alphabet.length) * options.alphabet.length;
	let written = 0;
	while (written < output.length) {
		const chunk = random.subarray(
			0,
			Math.min(output.length - written, random.length),
		);
		const filled = randomFillFn(chunk);
		if (filled.kind === "error") {
			output.fill(0);
			random.fill(0);
			return filled;
		}
		for (const byte of chunk) {
			if (byte < acceptedValues) {
				output[written] = options.alphabet.charCodeAt(
					byte % options.alphabet.length,
				);
				written += 1;
			}
		}
	}
	// Every byte is ASCII, so UTF-8 decoding preserves the selected characters.
	const text = new TextDecoder().decode(output);
	output.fill(0);
	random.fill(0);
	return createSuccessResult(text);
}
