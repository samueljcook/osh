import {
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../results/index.ts";
import type {
	CompiledFlatPattern,
	CompiledNestedPattern,
	PatternSegment,
} from "./compiled-pattern.ts";

/*
Compiles an authored pattern in a given pattern language into a compiled
pattern, or returns a syntax error. The compiler interprets the language's
markers here; the matching step in `./index.ts` receives only compiled
patterns. A consumer that only matches imports only the matching functions, so
its bundle can exclude the compiler.
*/

/**
 * The flat language: one dynamic-param prefix and one splat segment.
 * Each marker is a single non-whitespace code point other than a slash.
 */
export type FlatPatternLanguage = {
	readonly kind: "flat";
	readonly dynamicParamPrefix: string;
	readonly splatSegment: string;
};

/**
 * The nested language adds the segment that names a layout's index; that
 * segment is a non-empty static spelling with no slash or whitespace.
 */
export type NestedPatternLanguage = {
	readonly kind: "nested";
	readonly dynamicParamPrefix: string;
	readonly splatSegment: string;
	readonly explicitIndexSegment: string;
};

type PatternLanguage = FlatPatternLanguage | NestedPatternLanguage;

type InvalidDynamicParamPrefixError = {
	readonly kind: "invalid-dynamic-param-prefix";
	readonly dynamicParamPrefix: string;
};

type InvalidSplatSegmentError = {
	readonly kind: "invalid-splat-segment";
	readonly splatSegment: string;
};

type InvalidExplicitIndexSegmentError = {
	readonly kind: "invalid-explicit-index-segment";
	readonly explicitIndexSegment: string;
};

type PatternLanguageError =
	| InvalidDynamicParamPrefixError
	| InvalidSplatSegmentError
	| InvalidExplicitIndexSegmentError;

type EmptyPatternError = {
	readonly kind: "empty-pattern";
	readonly pattern: string;
};

type RelativePatternError = {
	readonly kind: "relative-pattern";
	readonly pattern: string;
};

type RepeatedSlashPatternError = {
	readonly kind: "repeated-slash-pattern";
	readonly pattern: string;
};

type WhitespacePatternError = {
	readonly kind: "whitespace-pattern";
	readonly pattern: string;
};

type InvalidDynamicParamNameError = {
	readonly kind: "invalid-dynamic-param-name";
	readonly paramName: string;
	readonly pattern: string;
};

type RepeatedDynamicParamNameError = {
	readonly kind: "repeated-dynamic-param-name";
	readonly paramName: string;
	readonly pattern: string;
};

type NonterminalSplatError = {
	readonly kind: "nonterminal-splat";
	readonly pattern: string;
};

type NestedTrailingSlashPatternError = {
	readonly kind: "nested-trailing-slash-pattern";
	readonly pattern: string;
};

type NonterminalIndexError = {
	readonly kind: "nonterminal-index";
	readonly pattern: string;
};

type PatternSyntaxError =
	| EmptyPatternError
	| RelativePatternError
	| RepeatedSlashPatternError
	| WhitespacePatternError
	| InvalidDynamicParamNameError
	| RepeatedDynamicParamNameError
	| NonterminalSplatError
	| NestedTrailingSlashPatternError
	| NonterminalIndexError;

export type PatternCompileError = PatternSyntaxError | PatternLanguageError;

/** A pattern compile error as a sentence fragment that names the pattern or the language setting at fault. */
export function formatPatternCompileError(error: PatternCompileError): string {
	switch (error.kind) {
		case "empty-pattern":
			return "the pattern is empty";
		case "relative-pattern":
			return `pattern ${JSON.stringify(error.pattern)} must begin with /`;
		case "repeated-slash-pattern":
			return `pattern ${JSON.stringify(error.pattern)} contains an empty segment`;
		case "whitespace-pattern":
			return `pattern ${JSON.stringify(error.pattern)} contains whitespace`;
		case "invalid-dynamic-param-name":
			return `pattern ${JSON.stringify(error.pattern)} names a dynamic param ${JSON.stringify(error.paramName)}, which is not an identifier`;
		case "repeated-dynamic-param-name":
			return `pattern ${JSON.stringify(error.pattern)} names dynamic param ${JSON.stringify(error.paramName)} more than once`;
		case "nonterminal-splat":
			return `pattern ${JSON.stringify(error.pattern)} has a splat that is not its last segment`;
		case "nested-trailing-slash-pattern":
			return `nested pattern ${JSON.stringify(error.pattern)} cannot end with /`;
		case "nonterminal-index":
			return `pattern ${JSON.stringify(error.pattern)} has an index segment that is not its last segment`;
		case "invalid-dynamic-param-prefix":
			return `the dynamic param prefix ${JSON.stringify(error.dynamicParamPrefix)} is invalid`;
		case "invalid-splat-segment":
			return `the splat segment ${JSON.stringify(error.splatSegment)} is invalid`;
		case "invalid-explicit-index-segment":
			return `the index segment ${JSON.stringify(error.explicitIndexSegment)} is invalid`;
	}
}

const DYNAMIC_PARAM_NAME_PATTERN = /^[A-Za-z_][A-Za-z0-9_]*$/u;

/**
 * Parses an absolute flat pattern without normalizing or decoding its text.
 * Repeated slashes and JavaScript whitespace are rejected. Dynamic names use
 * ASCII identifier syntax and must be unique within the pattern. A splat must
 * be the final segment; a trailing slash is a distinct non-splat terminal.
 * Language and syntax failures return PatternCompileError.
 */
export function compileFlatPattern(input: {
	readonly pattern: string;
	readonly language: FlatPatternLanguage;
}): Result<CompiledFlatPattern, PatternCompileError> {
	const { pattern } = input;
	const checked = validatePatternSyntax({
		pattern,
		language: input.language,
	});
	if (checked.kind === "error") {
		return createErrorResult(checked.error);
	}
	const hasTrailingSlash = pattern.length > 1 && pattern.endsWith("/");
	const segments = parsePatternSegments({
		pattern,
		language: input.language,
		values: splitSegments(
			hasTrailingSlash ? pattern.slice(0, -1) : pattern,
		),
	});
	if (segments.kind === "error") {
		return createErrorResult(segments.error);
	}
	if (hasTrailingSlash && segments.value.at(-1)?.kind === "splat") {
		return createErrorResult({ kind: "nonterminal-splat", pattern });
	}
	return createSuccessResult({
		kind: "flat",
		pattern,
		segments: segments.value,
		hasTrailingSlash,
	});
}

/**
 * Parses an absolute nested pattern under the supplied marker language.
 * The flat compiler's whitespace, slash, and dynamic-name rules also apply.
 * Non-root trailing slashes are rejected, and explicit index or splat segments
 * must be terminal. The compiler retains the authored spelling as its identity.
 */
export function compileNestedPattern(input: {
	readonly pattern: string;
	readonly language: NestedPatternLanguage;
}): Result<CompiledNestedPattern, PatternCompileError> {
	const { pattern } = input;
	const checked = validatePatternSyntax({
		pattern,
		language: input.language,
	});
	if (checked.kind === "error") {
		return createErrorResult(checked.error);
	}
	if (pattern.length > 1 && pattern.endsWith("/")) {
		return createErrorResult({
			kind: "nested-trailing-slash-pattern",
			pattern,
		});
	}
	const segments = parsePatternSegments({
		pattern,
		language: input.language,
		values: splitSegments(pattern),
	});
	if (segments.kind === "error") {
		return createErrorResult(segments.error);
	}
	return createSuccessResult({
		kind: "nested",
		pattern,
		segments: segments.value,
	});
}

/** The language's markers and the pattern's shape as a whole, before its segments are read. */
function validatePatternSyntax(input: {
	readonly pattern: string;
	readonly language: PatternLanguage;
}): Result<void, PatternCompileError> {
	const { pattern } = input;
	const { dynamicParamPrefix, splatSegment } = input.language;
	if (
		!testSingleNonWhitespaceCodePoint(dynamicParamPrefix) ||
		dynamicParamPrefix === "/"
	) {
		return createErrorResult({
			kind: "invalid-dynamic-param-prefix",
			dynamicParamPrefix,
		});
	}
	if (
		!testSingleNonWhitespaceCodePoint(splatSegment) ||
		splatSegment === "/"
	) {
		return createErrorResult({
			kind: "invalid-splat-segment",
			splatSegment,
		});
	}
	if (input.language.kind === "nested") {
		const { explicitIndexSegment } = input.language;
		if (
			explicitIndexSegment === "" ||
			explicitIndexSegment.includes("/") ||
			testForJsWhitespace(explicitIndexSegment) ||
			explicitIndexSegment === splatSegment ||
			explicitIndexSegment.startsWith(dynamicParamPrefix)
		) {
			return createErrorResult({
				kind: "invalid-explicit-index-segment",
				explicitIndexSegment,
			});
		}
	}
	if (pattern === "") {
		return createErrorResult({ kind: "empty-pattern", pattern });
	}
	if (!pattern.startsWith("/")) {
		return createErrorResult({ kind: "relative-pattern", pattern });
	}
	if (pattern.includes("//")) {
		return createErrorResult({ kind: "repeated-slash-pattern", pattern });
	}
	if (testForJsWhitespace(pattern)) {
		return createErrorResult({ kind: "whitespace-pattern", pattern });
	}
	return createSuccessResult(undefined);
}

/** The segment texts of an absolute pattern with no repeated slash; the root has none. */
function splitSegments(pattern: string): ReadonlyArray<string> {
	return pattern === "/" ? [] : pattern.slice(1).split("/");
}

function parsePatternSegments(input: {
	readonly pattern: string;
	readonly language: PatternLanguage;
	readonly values: ReadonlyArray<string>;
}): Result<ReadonlyArray<PatternSegment>, PatternSyntaxError> {
	const { pattern, language, values } = input;
	const segments: Array<PatternSegment> = [];
	const paramNames = new Set<string>();
	for (const [index, value] of values.entries()) {
		const isTerminal = index === values.length - 1;
		if (
			language.kind === "nested" &&
			value === language.explicitIndexSegment
		) {
			if (!isTerminal) {
				return createErrorResult({
					kind: "nonterminal-index",
					pattern,
				});
			}
			segments.push({ kind: "index" });
			continue;
		}
		if (value === language.splatSegment) {
			if (!isTerminal) {
				return createErrorResult({
					kind: "nonterminal-splat",
					pattern,
				});
			}
			segments.push({ kind: "splat" });
			continue;
		}
		if (value.startsWith(language.dynamicParamPrefix)) {
			const paramName = value.slice(language.dynamicParamPrefix.length);
			if (!DYNAMIC_PARAM_NAME_PATTERN.test(paramName)) {
				return createErrorResult({
					kind: "invalid-dynamic-param-name",
					paramName,
					pattern,
				});
			}
			if (paramNames.has(paramName)) {
				return createErrorResult({
					kind: "repeated-dynamic-param-name",
					paramName,
					pattern,
				});
			}
			paramNames.add(paramName);
			segments.push({ kind: "dynamic", paramName: paramName });
			continue;
		}
		segments.push({ kind: "static", value });
	}
	return createSuccessResult(segments);
}

function testForJsWhitespace(value: string): boolean {
	for (const character of value) {
		if (character.trim() === "") {
			return true;
		}
	}
	return false;
}

function testSingleNonWhitespaceCodePoint(value: string): boolean {
	return Array.from(value).length === 1 && !testForJsWhitespace(value);
}
