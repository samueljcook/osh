import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "primitives-ts/results";
import { PATTERN_PROTOCOL } from "./constants.ts";

/*
Path wire encoding. A request pathname, a route pattern, and a param each
have one canonical spelling: well-formed Unicode, percent-encoded with the
characters of `encodeWireText`. The matcher consumes only that spelling.
*/

export type PathWireError =
	| { readonly kind: "path-must-be-absolute"; readonly path: string }
	| {
			readonly kind: "malformed-percent-encoding";
			readonly segment: string;
			readonly offset: number;
	  }
	| {
			readonly kind: "invalid-percent-encoded-utf8";
			readonly segment: string;
	  }
	| {
			readonly kind: "malformed-unicode";
			readonly value: string;
			readonly offset: number;
	  }
	| { readonly kind: "empty-path-segment" }
	| { readonly kind: "dot-path-segment"; readonly value: "." | ".." }
	| {
			readonly kind: "path-segment-contains-separator";
			readonly segment: string;
	  }
	| {
			readonly kind: "static-dot-path-segment";
			readonly pattern: string;
			readonly segment: "." | "..";
	  };

/** Converts an absolute request pathname to the canonical spelling used for matching. */
export function canonicalizeRequestPath(
	path: string,
): Result<string, PathWireError> {
	if (!path.startsWith("/")) {
		return createErrorResult({ kind: "path-must-be-absolute", path });
	}
	const wellFormed = checkWellFormedUnicode(path);
	if (wellFormed.kind === "error") {
		return wellFormed;
	}
	const encoded: Array<string> = [];
	for (const segment of path.split("/")) {
		if (segment === "") {
			encoded.push("");
			continue;
		}
		const decoded = decodePercentSegment(segment);
		if (decoded.kind === "error") {
			return decoded;
		}
		encoded.push(encodeWireText(decoded.value));
	}
	return createSuccessResult(encoded.join("/"));
}

/** Encodes an authored route pattern without interpreting static percent escapes. */
export function encodeRoutePattern(
	pattern: string,
): Result<string, PathWireError> {
	if (!pattern.startsWith("/")) {
		return createErrorResult({
			kind: "path-must-be-absolute",
			path: pattern,
		});
	}
	const wellFormed = checkWellFormedUnicode(pattern);
	if (wellFormed.kind === "error") {
		return wellFormed;
	}
	const encoded: Array<string> = [];
	for (const segment of pattern.split("/")) {
		if (
			segment === "" ||
			segment === PATTERN_PROTOCOL.splatSegment ||
			segment.startsWith(PATTERN_PROTOCOL.dynamicParamPrefix)
		) {
			encoded.push(segment);
			continue;
		}
		if (segment === "." || segment === "..") {
			return createErrorResult({
				kind: "static-dot-path-segment",
				pattern,
				segment,
			});
		}
		encoded.push(encodeWireText(segment));
	}
	return createSuccessResult(encoded.join("/"));
}

export type DecodedMatch = {
	readonly params: Readonly<Record<string, string>>;
	readonly splatValues: ReadonlyArray<string>;
};

/**
 * Decodes a match's wire params to the logical values supplied to application
 * code. The params are an ordinary object, in which every param, including one
 * named `__proto__`, is an own property.
 */
export function decodeMatch(match: {
	readonly dynamicParams: Readonly<Record<string, string>>;
	readonly splatValues: ReadonlyArray<string>;
}): Result<DecodedMatch, PathWireError> {
	const paramEntries: Array<readonly [string, string]> = [];
	for (const [name, value] of Object.entries(match.dynamicParams)) {
		const decoded = decodePathSegment(value);
		if (decoded.kind === "error") {
			return decoded;
		}
		paramEntries.push([name, decoded.value]);
	}
	const params: Readonly<Record<string, string>> =
		Object.fromEntries(paramEntries);
	const splatValues: Array<string> = [];
	for (const value of match.splatValues) {
		const decoded = decodePathSegment(value);
		if (decoded.kind === "error") {
			return decoded;
		}
		splatValues.push(decoded.value);
	}
	return createSuccessResult({ params, splatValues });
}

/** Encodes one non-empty logical dynamic or splat value as one wire segment. */
export function encodePathSegment(
	value: string,
): Result<string, PathWireError> {
	if (value === "") {
		return createErrorResult({ kind: "empty-path-segment" });
	}
	if (value === "." || value === "..") {
		return createErrorResult({ kind: "dot-path-segment", value });
	}
	const wellFormed = checkWellFormedUnicode(value);
	if (wellFormed.kind === "error") {
		return wellFormed;
	}
	return createSuccessResult(encodeWireText(value));
}

/** Decodes one matched wire segment to the logical value supplied to application code. */
export function decodePathSegment(
	segment: string,
): Result<string, PathWireError> {
	if (segment === "") {
		return createErrorResult({ kind: "empty-path-segment" });
	}
	if (segment.includes("/")) {
		return createErrorResult({
			kind: "path-segment-contains-separator",
			segment,
		});
	}
	const decoded = decodePercentSegment(segment);
	if (decoded.kind === "error") {
		return decoded;
	}
	return decoded.value === "." || decoded.value === ".."
		? createErrorResult({ kind: "dot-path-segment", value: decoded.value })
		: decoded;
}

export function formatPathWireError(error: PathWireError): string {
	switch (error.kind) {
		case "path-must-be-absolute":
			return `path ${JSON.stringify(error.path)} must begin with /`;
		case "malformed-percent-encoding":
			return `malformed percent encoding at offset ${error.offset} of path segment ${JSON.stringify(error.segment)}`;
		case "invalid-percent-encoded-utf8":
			return `percent-decoded path segment is not UTF-8: ${JSON.stringify(error.segment)}`;
		case "malformed-unicode":
			return `value contains an unpaired Unicode surrogate at offset ${error.offset}`;
		case "empty-path-segment":
			return "path segment cannot be empty";
		case "dot-path-segment":
			return `path segment ${JSON.stringify(error.value)} would be erased by URL normalization`;
		case "path-segment-contains-separator":
			return "one path wire segment cannot contain a literal / separator";
		case "static-dot-path-segment":
			return `route pattern ${JSON.stringify(error.pattern)} contains static segment ${JSON.stringify(error.segment)}, which URL normalization would erase`;
	}
}

/** Returns an error for a value with an unpaired surrogate, which is not well-formed Unicode. */
export function checkWellFormedUnicode(
	value: string,
): Result<void, PathWireError> {
	for (let offset = 0; offset < value.length; offset++) {
		const unit = value.charCodeAt(offset);
		if (unit >= 0xd800 && unit <= 0xdbff) {
			const next = value.charCodeAt(offset + 1);
			if (!(next >= 0xdc00 && next <= 0xdfff)) {
				return createErrorResult({
					kind: "malformed-unicode",
					value,
					offset,
				});
			}
			offset += 1;
		} else if (unit >= 0xdc00 && unit <= 0xdfff) {
			return createErrorResult({
				kind: "malformed-unicode",
				value,
				offset,
			});
		}
	}
	return createSuccessResult(undefined);
}

function decodePercentSegment(segment: string): Result<string, PathWireError> {
	const wellFormed = checkWellFormedUnicode(segment);
	if (wellFormed.kind === "error") {
		return wellFormed;
	}
	for (let offset = 0; offset < segment.length; offset++) {
		if (segment[offset] === "%") {
			const high = toHexValue(segment.charCodeAt(offset + 1));
			const low = toHexValue(segment.charCodeAt(offset + 2));
			if (high === null || low === null) {
				return createErrorResult({
					kind: "malformed-percent-encoding",
					segment,
					offset,
				});
			}
			offset += 2;
		}
	}
	const decoded = callWithResult(() => decodeURIComponent(segment));
	return decoded.kind === "ok"
		? decoded
		: createErrorResult({ kind: "invalid-percent-encoded-utf8", segment });
}

function encodeWireText(value: string): string {
	return encodeURIComponent(value)
		.replaceAll("%2B", "+")
		.replace(/[!'()*]/g, (character) => {
			return `%${character.charCodeAt(0).toString(16).toUpperCase()}`;
		});
}

function toHexValue(unit: number): number | null {
	if (unit >= 0x30 && unit <= 0x39) {
		return unit - 0x30;
	}
	if (unit >= 0x41 && unit <= 0x46) {
		return unit - 0x41 + 10;
	}
	if (unit >= 0x61 && unit <= 0x66) {
		return unit - 0x61 + 10;
	}
	return null;
}
