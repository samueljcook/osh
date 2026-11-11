import type { Result } from "../../results/index.ts";
import { createErrorResult, createSuccessResult } from "../../results/index.ts";

/** A malformed UTF-8 byte sequence or an unpaired JavaScript surrogate. */
export type Utf8Error =
	| { readonly kind: "invalid-utf8"; readonly byteIndex: number }
	| { readonly kind: "unpaired-surrogate"; readonly codeUnitIndex: number };

/** Encodes JavaScript text as UTF-8, with explicit handling of unpaired surrogates. */
export function encodeUtf8(
	text: string,
	options: { readonly invalidText: "reject" | "replace" },
): Result<Uint8Array, Utf8Error> {
	if (options.invalidText === "reject") {
		for (let index = 0; index < text.length; index++) {
			const code = text.charCodeAt(index);
			if (code < 0xd800 || code > 0xdfff) {
				continue;
			}
			const next = text.charCodeAt(index + 1);
			if (code <= 0xdbff && next >= 0xdc00 && next <= 0xdfff) {
				index++;
				continue;
			}
			return createErrorResult({
				kind: "unpaired-surrogate",
				codeUnitIndex: index,
			});
		}
	}
	return createSuccessResult(new TextEncoder().encode(text));
}

/**
 * Decodes UTF-8 with explicit malformed-input handling. An initial U+FEFF is
 * preserved as text. Replacement uses the WHATWG Encoding Standard's U+FFFD
 * behavior. No Unicode normalization is performed.
 */
export function decodeUtf8(
	bytes: Uint8Array,
	options: { readonly invalidBytes: "reject" | "replace" },
): Result<string, Utf8Error> {
	if (options.invalidBytes === "reject") {
		for (let index = 0; index < bytes.length;) {
			const first = bytes[index]!;
			if (first < 0x80) {
				index++;
				continue;
			}
			const size =
				first >= 0xc2 && first <= 0xdf
					? 2
					: first >= 0xe0 && first <= 0xef
						? 3
						: first >= 0xf0 && first <= 0xf4
							? 4
							: 0;
			if (size === 0 || index + size > bytes.length) {
				return createErrorResult({
					kind: "invalid-utf8",
					byteIndex: index,
				});
			}
			const second = bytes[index + 1]!;
			if (
				(first === 0xe0 && second < 0xa0) ||
				(first === 0xed && second >= 0xa0) ||
				(first === 0xf0 && second < 0x90) ||
				(first === 0xf4 && second >= 0x90)
			) {
				return createErrorResult({
					kind: "invalid-utf8",
					byteIndex: index,
				});
			}
			for (let offset = 1; offset < size; offset++) {
				const byte = bytes[index + offset]!;
				if (byte < 0x80 || byte > 0xbf) {
					return createErrorResult({
						kind: "invalid-utf8",
						byteIndex: index,
					});
				}
			}
			index += size;
		}
	}
	return createSuccessResult(
		new TextDecoder("utf-8", { ignoreBOM: true }).decode(bytes),
	);
}
