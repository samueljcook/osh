import { decodeUtf8 } from "../../../encoding/utf8/index.ts";
import type { Result } from "../../../results/index.ts";
import {
	createErrorResult,
	createSuccessResult,
} from "../../../results/index.ts";
import { COOKIE_NAME_PATTERN, COOKIE_VALUE_PATTERN } from "../syntax.ts";
const MAX_HEADER_SIZE = 16 * 1024;
/** A cookie operation failure. Errors contain no cookie values. */
export type CookieError = {
	readonly kind:
		| "invalid-name"
		| "invalid-value"
		| "invalid-path"
		| "invalid-domain"
		| "invalid-max-age"
		| "same-site-requires-secure"
		| "cookie-too-large"
		| "header-too-large"
		| "malformed-header"
		| "duplicate-cookie"
		| "invalid-percent-encoding"
		| "invalid-utf8"
		| "cookie-access-denied";
};

/**
 * Parses a unique cookie value without accessing the browser. Missing values
 * return undefined; empty values remain empty strings. The header must contain
 * fewer than 16,384 UTF-16 code units. Parsing splits the header at every `;`,
 * removes spaces and tabs around each piece, and splits each piece at its first
 * `=`. Pieces without `=` and pairs with other names never cause an error, even
 * when their names or values are outside the cookie grammar. Two or more pairs
 * with the requested name return `duplicate-cookie`, whatever their values are.
 * One pair with the requested name and a value outside the cookie grammar
 * returns `malformed-header`. One pair of surrounding double quotes is removed
 * from the value. Percent decoding applies only to the requested value and
 * rejects malformed escapes or UTF-8; plus signs remain literal plus signs.
 */
export function parseCookieValue(
	header: string,
	options: { readonly name: string; readonly encoding: "raw" | "percent" },
): Result<string | undefined, CookieError> {
	if (!COOKIE_NAME_PATTERN.test(options.name)) {
		return createErrorResult({ kind: "invalid-name" });
	}
	if (header.length + 1 > MAX_HEADER_SIZE) {
		return createErrorResult({ kind: "header-too-large" });
	}
	let requested:
		| { readonly kind: "absent" }
		| { readonly kind: "valid"; readonly value: string }
		| { readonly kind: "invalid" } = { kind: "absent" };
	for (const rawPair of header.split(";")) {
		const pair = rawPair.replace(/^[ \t]+|[ \t]+$/g, "");
		const separator = pair.indexOf("=");
		// The requested name is valid, so a pair with an invalid name never matches it.
		if (separator < 0 || pair.slice(0, separator) !== options.name) {
			continue;
		}
		if (requested.kind !== "absent") {
			return createErrorResult({ kind: "duplicate-cookie" });
		}
		let value = pair.slice(separator + 1);
		if (value.startsWith('"') && value.endsWith('"') && value.length >= 2) {
			value = value.slice(1, -1);
		}
		requested = COOKIE_VALUE_PATTERN.test(value)
			? { kind: "valid", value }
			: { kind: "invalid" };
	}
	switch (requested.kind) {
		case "absent":
			return createSuccessResult(undefined);
		case "invalid":
			return createErrorResult({ kind: "malformed-header" });
		case "valid":
			return options.encoding === "raw"
				? createSuccessResult(requested.value)
				: decodePercentText(requested.value);
	}
}

function decodePercentText(text: string): Result<string, CookieError> {
	const bytes: Array<number> = [];
	for (let index = 0; index < text.length; index++) {
		if (text[index] === "%") {
			const hex = text.slice(index + 1, index + 3);
			if (!/^[0-9A-Fa-f]{2}$/.test(hex)) {
				return createErrorResult({ kind: "invalid-percent-encoding" });
			}
			bytes.push(Number.parseInt(hex, 16));
			index += 2;
		} else {
			bytes.push(text.charCodeAt(index));
		}
	}
	const decoded = decodeUtf8(Uint8Array.from(bytes), {
		invalidBytes: "reject",
	});
	return decoded.kind === "ok"
		? decoded
		: createErrorResult({ kind: "invalid-utf8" });
}
