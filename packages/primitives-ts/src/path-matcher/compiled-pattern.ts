/*
The compiled form of a route pattern: what the compile step produces and the
matching step consumes. It is plain data that serializes as JSON, so a build
can compile patterns once and send them to a runtime that never reads the
pattern language.
*/

type StaticSegment = {
	readonly kind: "static";
	/** The segment text as authored, matched to a path segment by equality. */
	readonly value: string;
};

type DynamicSegment = {
	readonly kind: "dynamic";
	readonly paramName: string;
};

/** Consumes every remaining path segment; always the last segment of its pattern. */
type SplatSegment = { readonly kind: "splat" };

/**
 * Names the index of the path that its preceding segments spell; an index
 * segment appears only in nested patterns and is always the last segment of its
 * pattern.
 */
type IndexSegment = { readonly kind: "index" };

export type PatternSegment =
	| StaticSegment
	| DynamicSegment
	| SplatSegment
	| IndexSegment;

export type CompiledFlatPattern = {
	readonly kind: "flat";
	/** Records the authored pattern text that a match reports as its pattern identity. */
	readonly pattern: string;
	/** Contains no index segment because the flat language has none. */
	readonly segments: ReadonlyArray<PatternSegment>;
	/** The pattern ends in a slash after its segments and matches only paths that do. */
	readonly hasTrailingSlash: boolean;
};

export type CompiledNestedPattern = {
	readonly kind: "nested";
	/** Records the authored pattern text that a match reports as its pattern identity. */
	readonly pattern: string;
	readonly segments: ReadonlyArray<PatternSegment>;
};

export type CompiledPattern = CompiledFlatPattern | CompiledNestedPattern;
