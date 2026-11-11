import {
	encodeRoutePattern,
	formatPathWireError,
	type PathWireError,
} from "@vorma/client/unstable";
import { Result } from "effect";
import {
	compileNestedPattern,
	type CompiledNestedPattern,
	formatPatternCompileError,
	type PatternCompileError,
} from "primitives-ts/path-matcher";
import { NESTED_PATTERN_LANGUAGE } from "./pattern-language.ts";

/** The step that rejected a route pattern and that step's error. */
export type RoutePatternError =
	| { readonly _tag: "PathWire"; readonly error: PathWireError }
	| { readonly _tag: "PatternCompile"; readonly error: PatternCompileError };

/**
 * Compiles an authored view route pattern: wire encoding first, then the
 * nested pattern language. Every view pattern enters the matcher through this
 * function.
 */
export function compileViewPattern(
	pattern: string,
): Result.Result<CompiledNestedPattern, RoutePatternError> {
	const encoded = encodeRoutePattern(pattern);
	if (encoded.kind === "error") {
		return Result.fail({ _tag: "PathWire", error: encoded.error });
	}
	const compiled = compileNestedPattern({
		pattern: encoded.value,
		language: NESTED_PATTERN_LANGUAGE,
	});
	return compiled.kind === "ok"
		? Result.succeed(compiled.value)
		: Result.fail({ _tag: "PatternCompile", error: compiled.error });
}

/** A route pattern's compile error as a sentence fragment. */
export function formatRoutePatternError(error: RoutePatternError): string {
	switch (error._tag) {
		case "PathWire":
			return formatPathWireError(error.error);
		case "PatternCompile":
			return formatPatternCompileError(error.error);
	}
}
