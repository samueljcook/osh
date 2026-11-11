import {
	generateRandomTextId,
	RANDOM_TEXT_ID_ALPHABETS,
} from "primitives-ts/id/random-text";

/** An opaque 128-bit random identity, also available on HTTP origins. */
export function createRandomId(): string {
	const result = generateRandomTextId({
		alphabet: RANDOM_TEXT_ID_ALPHABETS.lowercaseHex,
		length: 32,
	});
	if (result.kind === "error") {
		// Browser client identities require working platform randomness.
		throw new Error("The browser client cannot generate an identity", {
			cause: result.error,
		});
	}
	return result.value;
}
