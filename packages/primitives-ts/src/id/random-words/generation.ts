import { ENGLISH_WORDS } from "../../internal/english-words.ts";
import type { RandomBytesError } from "../../random/secure-bytes/index.ts";
import type { Result } from "../../results/index.ts";
import { createErrorResult, createSuccessResult } from "../../results/index.ts";
import type { RandomWordIdError } from "./index.ts";
import type { RandomWordIdOptions } from "./index.ts";

export function generateRandomWordIdWithSource(
	options: RandomWordIdOptions,
	randomFillFn: (
		output: Uint8Array<ArrayBuffer>,
	) => Result<void, RandomBytesError>,
): Result<string, RandomWordIdError> {
	if (
		!Number.isSafeInteger(options.wordCount) ||
		options.wordCount <= 0 ||
		options.wordCount > Math.floor(Number.MAX_SAFE_INTEGER / 9)
	) {
		return createErrorResult({ kind: "invalid-length" });
	}
	const words = Array.from({ length: options.wordCount }, () => "");
	const random = new Uint8Array(Math.min(options.wordCount, 128) * 2);
	let written = 0;
	while (written < options.wordCount) {
		const count = Math.min(options.wordCount - written, random.length / 2);
		const chunk = random.subarray(0, count * 2);
		const filled = randomFillFn(chunk);
		if (filled.kind === "error") {
			words.fill("");
			random.fill(0);
			return filled;
		}
		for (let index = 0; index < chunk.length; index += 2) {
			// Each 11-bit index has 32 preimages among uniformly random 16-bit samples.
			const wordIndex = (chunk[index]! | (chunk[index + 1]! << 8)) & 2047;
			words[written] = ENGLISH_WORDS[wordIndex]!;
			written += 1;
		}
	}
	random.fill(0);
	return createSuccessResult(words.join("-"));
}
