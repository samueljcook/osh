import { encodeUtf8 } from "../../../encoding/utf8/index.ts";
import type { Result } from "../../../results/index.ts";
import {
	callWithResult,
	flatMapResult,
	mapResultError,
	createErrorResult,
	createSuccessResult,
} from "../../../results/index.ts";
import type { CookieError } from "../headers/index.ts";
import { parseCookieValue } from "../headers/index.ts";
import { COOKIE_NAME_PATTERN, COOKIE_VALUE_PATTERN } from "../syntax.ts";

/** Browser scope, with either a managed host prefix or explicit path/domain. */
export type CookieScope =
	| { readonly kind: "host-only" }
	| {
			readonly kind: "explicit";
			readonly path: string;
			readonly domain: string | undefined;
	  };

/** Explicit browser policy. Browser-created cookies cannot be HttpOnly. */
export type BrowserCookieOptions = {
	readonly name: string;
	readonly scope: CookieScope;
	/**
	 * `https` writes Secure and uses `__Host-` for host-only scope.
	 * `loopback-http` adapts the attributes for local development over loopback
	 * HTTP: it writes neither Secure nor Partitioned, uses `__Dev-` for host-only
	 * scope, and rejects `sameSite: "none"`.
	 */
	readonly security: "https" | "loopback-http";
	readonly sameSite: "lax" | "strict" | "none";
	/**
	 * Writes Partitioned under `https`. Under `loopback-http`, the writer omits
	 * Partitioned even when this value is true.
	 */
	readonly partitioned: boolean;
	/** Undefined selects a browser-session cookie; supplied lifetimes must be positive integers. */
	readonly maxAgeSeconds: number | undefined;
	/** Percent encoding supports arbitrary Unicode text; raw values use cookie syntax directly. */
	readonly encoding: "raw" | "percent";
};

const cookieDefinitionBrand: unique symbol = Symbol(
	"browser-cookie-definition",
);

/** Immutable creation/deletion scope. Construct it with defineBrowserCookie. */
export type BrowserCookieDefinition = {
	readonly [cookieDefinitionBrand]: true;
	readonly name: string;
	readonly encoding: "raw" | "percent";
	readonly attrs: string;
	readonly maxAgeSeconds: number | undefined;
};

const MAX_COOKIE_SIZE = 4096;
const COOKIE_ATTRIBUTE_PATTERN = /^[\x20-\x3a\x3c-\x7e]*$/;

/**
 * Parses browser cookie policy and resolves its final name and scope.
 * Host-only scope uses __Host- over HTTPS and __Dev- over loopback HTTP, with
 * Path=/ and no Domain. Explicit scope retains the supplied unprefixed name.
 * Loopback HTTP disables Secure and Partitioned for either scope; applications
 * must use that transport mode only for local development. A name that starts
 * with `__Host-`, `__Dev-`, or `__Secure-` in any ASCII case is invalid.
 * Definition construction does not access document or write a cookie.
 */
export function defineBrowserCookie(
	options: BrowserCookieOptions,
): Result<BrowserCookieDefinition, CookieError> {
	if (
		!COOKIE_NAME_PATTERN.test(options.name) ||
		/^(?:__Host-|__Dev-|__Secure-)/i.test(options.name)
	) {
		return createErrorResult({ kind: "invalid-name" });
	}
	const secure = options.security === "https";
	if (!secure && options.sameSite === "none") {
		return createErrorResult({ kind: "same-site-requires-secure" });
	}
	if (
		options.maxAgeSeconds !== undefined &&
		(!Number.isSafeInteger(options.maxAgeSeconds) ||
			options.maxAgeSeconds <= 0)
	) {
		return createErrorResult({ kind: "invalid-max-age" });
	}
	let name = options.name;
	let path = "/";
	let domain: string | undefined;
	if (options.scope.kind === "host-only") {
		name = `${secure ? "__Host-" : "__Dev-"}${name}`;
	} else {
		path = options.scope.path;
		if (!path.startsWith("/") || !COOKIE_ATTRIBUTE_PATTERN.test(path)) {
			return createErrorResult({ kind: "invalid-path" });
		}
		domain = options.scope.domain?.replace(/^\./, "").toLowerCase();
		if (
			domain !== undefined &&
			(domain.length === 0 ||
				domain.length > 253 ||
				domain.split(".").some((label) => {
					return (
						label.length === 0 ||
						label.length > 63 ||
						!/^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$/.test(label)
					);
				}))
		) {
			return createErrorResult({ kind: "invalid-domain" });
		}
	}
	const attrs = `; Path=${path}${domain === undefined ? "" : `; Domain=${domain}`}; SameSite=${options.sameSite === "none" ? "None" : options.sameSite === "lax" ? "Lax" : "Strict"}${secure ? "; Secure" : ""}${secure && options.partitioned ? "; Partitioned" : ""}`;
	const definition: BrowserCookieDefinition = Object.freeze({
		[cookieDefinitionBrand]: true as const,
		name,
		encoding: options.encoding,
		attrs,
		maxAgeSeconds: options.maxAgeSeconds,
	});
	if (
		serializeCookie(definition, "", false).length > MAX_COOKIE_SIZE ||
		serializeCookie(definition, "", true).length > MAX_COOKIE_SIZE
	) {
		return createErrorResult({ kind: "cookie-too-large" });
	}
	return createSuccessResult(definition);
}

/** Reads a unique value from document.cookie using the selected encoding. */
export function getCookieValue(options: {
	readonly name: string;
	readonly encoding: "raw" | "percent";
}): Result<string | undefined, CookieError> {
	const text = mapResultError(
		callWithResult(() => document.cookie),
		(): CookieError => {
			return { kind: "cookie-access-denied" };
		},
	);
	return flatMapResult(text, (value) => parseCookieValue(value, options));
}

/**
 * Writes one cookie using its validated scope and selected value encoding.
 * Success reports an accepted document.cookie assignment; browser storage
 * policy can still reject the cookie without reporting an error.
 */
export function setCookieValue(
	definition: BrowserCookieDefinition,
	value: string,
): Result<void, CookieError> {
	const encoded =
		definition.encoding === "raw"
			? createSuccessResult(value)
			: encodePercentText(value);
	if (encoded.kind === "error") {
		return encoded;
	}
	if (!COOKIE_VALUE_PATTERN.test(encoded.value)) {
		return createErrorResult({ kind: "invalid-value" });
	}
	return writeCookie(serializeCookie(definition, encoded.value, false));
}

/** Writes Max-Age=0 using the definition's exact name, path, domain, and partition policy. */
export function deleteCookie(
	definition: BrowserCookieDefinition,
): Result<void, CookieError> {
	return writeCookie(serializeCookie(definition, "", true));
}

function serializeCookie(
	definition: BrowserCookieDefinition,
	value: string,
	remove: boolean,
): string {
	const maxAge = remove ? 0 : definition.maxAgeSeconds;
	return `${definition.name}=${value}${definition.attrs}${maxAge === undefined ? "" : `; Max-Age=${maxAge}`}`;
}
function writeCookie(text: string): Result<void, CookieError> {
	if (text.length > MAX_COOKIE_SIZE) {
		return createErrorResult({ kind: "cookie-too-large" });
	}
	return mapResultError(
		callWithResult(() => {
			document.cookie = text;
		}),
		(): CookieError => {
			return { kind: "cookie-access-denied" };
		},
	);
}
function encodePercentText(text: string): Result<string, CookieError> {
	if (text.length > MAX_COOKIE_SIZE) {
		return createErrorResult({ kind: "cookie-too-large" });
	}
	const bytes = encodeUtf8(text, { invalidText: "reject" });
	if (bytes.kind === "error") {
		return createErrorResult({ kind: "invalid-utf8" });
	}
	let encoded = "";
	for (const byte of bytes.value) {
		const character = String.fromCharCode(byte);
		encoded += /^[A-Za-z0-9\-_.!~*'()]$/.test(character)
			? character
			: `%${byte.toString(16).toUpperCase().padStart(2, "0")}`;
		if (encoded.length > MAX_COOKIE_SIZE) {
			return createErrorResult({ kind: "cookie-too-large" });
		}
	}
	return createSuccessResult(encoded);
}
