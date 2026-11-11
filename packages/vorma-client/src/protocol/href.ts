import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "primitives-ts/results";
import { HTTP_PROTOCOL, PATTERN_PROTOCOL } from "./constants.ts";
import {
	encodePathSegment,
	encodeRoutePattern,
	formatPathWireError,
	type PathWireError,
} from "./path.ts";
import { serializeSearch } from "./search.ts";

/*
An href from a pattern literal and its params. The literal is split at
runtime, a `:name` segment takes the param of that name, `*` takes the
splat values, and, for a view pattern, a trailing `_index` names its parent path
and is omitted. The build validated every pattern that an application names,
so nothing here checks syntax; static segments are percent-encoded into their
canonical spelling and params are encoded as path segments.
*/

export type HrefError = {
	readonly kind: "href-error";
	readonly msg: string;
};

/**
 * The search of an href: none; the search input of a target or a call, which
 * the grammar serializes; or query text that the caller already wrote in the
 * grammar, without its leading question mark.
 */
export type HrefSearch =
	| { readonly kind: "none" }
	| { readonly kind: "input"; readonly input: unknown }
	| { readonly kind: "query-text"; readonly text: string };

/**
 * Builds an href. The query spells the whole search input: every field that
 * is not `undefined`, whether or not a loader or an endpoint declares it. A
 * search input is `undefined` or an object of fields; any other value, such
 * as a string, is an error.
 */
export function buildHref(input: {
	/** A view pattern collapses its index segment; an endpoint pattern keeps a trailing slash. */
	readonly kind: "view" | "endpoint";
	readonly pattern: string;
	readonly params?: Readonly<Record<string, string>> | undefined;
	readonly splatValues?: ReadonlyArray<string> | undefined;
	readonly search: HrefSearch;
	readonly hash?: string | undefined;
	/** Whether Vercel skew protection is on, which makes `dpl` a reserved search name. */
	readonly skewProtectionEnabled: boolean;
}): Result<string, HrefError> {
	const encodedPattern = encodeRoutePattern(input.pattern);
	if (encodedPattern.kind === "error") {
		return failPathWire(encodedPattern.error);
	}
	const encodedPatternText = encodedPattern.value;
	const hasTrailingSlash =
		input.kind === "endpoint" &&
		encodedPatternText.length > 1 &&
		encodedPatternText.endsWith("/");
	const patternSegments =
		encodedPatternText === "/"
			? []
			: encodedPatternText
					.slice(1, hasTrailingSlash ? -1 : undefined)
					.split("/");
	let trailingSlash = hasTrailingSlash;
	const params: Readonly<Record<string, string>> = input.params ?? {};
	const splatValues: ReadonlyArray<string> = input.splatValues ?? [];
	const segments: Array<string> = [];
	let hasSplat = false;
	for (const [index, segment] of patternSegments.entries()) {
		const isLast = index === patternSegments.length - 1;
		if (
			input.kind === "view" &&
			isLast &&
			segment === PATTERN_PROTOCOL.indexSegment
		) {
			continue;
		}
		if (segment === PATTERN_PROTOCOL.splatSegment) {
			hasSplat = true;
			if (splatValues.length === 0) {
				if (input.kind === "view" && patternSegments.length > 1) {
					return failHref(
						`view pattern ${JSON.stringify(input.pattern)} cannot select its non-root splat with no values`,
					);
				}
				if (input.kind === "endpoint" && segments.length > 0) {
					trailingSlash = true;
				}
			}
			for (const splatValue of splatValues) {
				const encoded = encodePathSegment(splatValue);
				if (encoded.kind === "error") {
					return failPathWire(encoded.error);
				}
				segments.push(encoded.value);
			}
			continue;
		}
		if (segment.startsWith(PATTERN_PROTOCOL.dynamicParamPrefix)) {
			const paramName = segment.slice(
				PATTERN_PROTOCOL.dynamicParamPrefix.length,
			);
			// An own property only: `{}` carries `constructor` and `toString`
			// from its prototype, and neither is a param.
			const value = Object.hasOwn(params, paramName)
				? params[paramName]
				: undefined;
			if (value === undefined) {
				return failHref(
					`missing param ${paramName} for ${input.pattern}`,
				);
			}
			const encoded = encodePathSegment(value);
			if (encoded.kind === "error") {
				return failPathWire(encoded.error);
			}
			segments.push(encoded.value);
			continue;
		}
		segments.push(segment);
	}
	if (!hasSplat && splatValues.length > 0) {
		return failHref(
			`pattern ${JSON.stringify(input.pattern)} does not accept splat values`,
		);
	}
	const path =
		segments.length === 0
			? "/"
			: `/${segments.join("/")}${trailingSlash ? "/" : ""}`;
	const query = toQueryString(input.search, input.skewProtectionEnabled);
	if (query.kind === "error") {
		return query;
	}
	return createSuccessResult(`${path}${query.value}${toHash(input.hash)}`);
}

function toHash(hash: string | undefined): string {
	if (hash === undefined || hash === "") {
		return "";
	}
	const base = "about:blank";
	const url = new URL(base);
	url.hash = hash;
	// `URL.hash` reads an explicitly empty hash as an empty string;
	// the serialized URL preserves its authored `#`.
	return url.href.slice(base.length);
}

/**
 * The top-level search names that an application cannot use: Vorma's
 * `vorma-json`, and Vercel's `dpl` while skew protection is on.
 */
function listReservedSearchNames(
	skewProtectionEnabled: boolean,
): ReadonlyArray<string> {
	return skewProtectionEnabled
		? [
				HTTP_PROTOCOL.queryKeys.viewPayload,
				HTTP_PROTOCOL.queryKeys.deployment,
			]
		: [HTTP_PROTOCOL.queryKeys.viewPayload];
}

/**
 * Query text is used as given. A search input is serialized in the search
 * grammar. A route search is a record of fields, and one with no fields
 * present is the absence of a query, not the grammar's spelling of an empty
 * record. A query that carries a reserved search name is an error, because the
 * server removes the key before an application reads it.
 */
function toQueryString(
	search: HrefSearch,
	skewProtectionEnabled: boolean,
): Result<string, HrefError> {
	const query = serializeQuery(search);
	if (query.kind === "error") {
		return query;
	}
	const reservedNames = listReservedSearchNames(skewProtectionEnabled);
	const reserved = reservedNames.find((name) =>
		new URLSearchParams(query.value).has(name),
	);
	if (reserved !== undefined) {
		return failHref(
			`the search parameter ${JSON.stringify(reserved)} is reserved${
				reserved === HTTP_PROTOCOL.queryKeys.deployment
					? " by Vercel skew protection"
					: " for Vorma's wire protocol"
			}; the reserved names are ${reservedNames.map((name) => JSON.stringify(name)).join(" and ")}`,
		);
	}
	return query;
}

function serializeQuery(search: HrefSearch): Result<string, HrefError> {
	switch (search.kind) {
		case "none":
			return createSuccessResult("");
		case "query-text":
			return createSuccessResult(
				search.text === "" ? "" : `?${search.text}`,
			);
		case "input":
			return serializeSearchInput(search.input);
	}
}

function serializeSearchInput(input: unknown): Result<string, HrefError> {
	if (input === undefined) {
		return createSuccessResult("");
	}
	if (typeof input !== "object" || input === null || Array.isArray(input)) {
		return failHref(
			"a search input must be an object of fields or undefined",
		);
	}
	const isEmpty = callWithResult(() =>
		Object.values(input).every((field) => field === undefined),
	);
	if (isEmpty.kind === "error") {
		return failHref("the fields of the search input could not be read");
	}
	if (isEmpty.value) {
		return createSuccessResult("");
	}
	const serialized = serializeSearch(input);
	if (serialized.kind === "error") {
		return failHref(
			`search parameters cannot be serialized: ${serialized.error.msg}`,
		);
	}
	return createSuccessResult(
		serialized.value === "" ? "" : `?${serialized.value}`,
	);
}

function failHref(msg: string): Result<never, HrefError> {
	return createErrorResult({ kind: "href-error", msg });
}

function failPathWire(cause: PathWireError): Result<never, HrefError> {
	return failHref(formatPathWireError(cause));
}
