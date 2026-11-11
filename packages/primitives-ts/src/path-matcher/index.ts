import {
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../results/index.ts";
import type {
	CompiledFlatPattern,
	CompiledNestedPattern,
	CompiledPattern,
} from "./compiled-pattern.ts";

export type {
	CompiledFlatPattern,
	CompiledNestedPattern,
	CompiledPattern,
	PatternSegment,
} from "./compiled-pattern.ts";
export {
	compileFlatPattern,
	compileNestedPattern,
	formatPatternCompileError,
	type FlatPatternLanguage,
	type NestedPatternLanguage,
	type PatternCompileError,
} from "./pattern-compiler.ts";

/*
Compiled patterns become a segment tree, and a path selects its best match.
The compiler in `./pattern-compiler.ts` parses authored patterns. Browser
applications can consume compiled patterns without including that compiler.

A flat matcher answers with one pattern. A nested matcher answers with the
chain of route patterns above the winning leaf, outermost first, each with
the parameters that its own pattern names.

Two patterns occupy the same matching position when they have the same static
segment text, dynamic segments in the same places, and the same final marker,
if any: a trailing slash or a splat for a flat pattern, and an index segment
or a splat for a nested pattern. Such patterns match the same paths, even
when their parameter names differ, so a matcher could select only one of
them. Every constructor returns a `DuplicatePositionError` for them, as the
constructors of the Rust `primitives_rs::path_matcher` matchers do.

Selection prefers a static child over a dynamic child over a splat at every
depth, and backtracks to the alternatives that it passed when the preferred
branch cannot complete. At the end of the path, a flat matcher prefers a
trailing-slash route when the path has one, then the plain route, then a
splat that may consume nothing (at the root, or after a trailing slash); a
nested matcher prefers the index, then the root splat, then the plain route.
*/

type DynamicParams = Readonly<Record<string, string>>;

export type FlatMatch = {
	readonly pattern: string;
	readonly dynamicParams: DynamicParams;
	readonly splatValues: ReadonlyArray<string>;
	/** The normalized path ends in a slash after its segments. */
	readonly hasTrailingSlash: boolean;
};

/** Describes one matched position in a nested match, including its pattern and the parameters that the pattern names. */
export type NestedMatch = {
	readonly pattern: string;
	readonly dynamicParams: DynamicParams;
	readonly splatValues: ReadonlyArray<string>;
};

/** Orders matched positions from outermost to innermost; the last position is the leaf that completed the path. */
export type NestedMatches = ReadonlyArray<NestedMatch>;

type MatcherKind = "flat" | "nested";
type TerminalKind = "route" | "trailing-route" | "splat" | "index";

/** Stores a compiled pattern in the segment tree with its parameters resolved to segment indexes. */
type TreePattern<Payload extends object> = {
	readonly payload: Payload;
	/** The pattern's index among the registrations that the constructor received. */
	readonly registrationIndex: number;
	readonly pattern: string;
	readonly consumedSegmentCount: number;
	readonly paramIndexes: ReadonlyArray<number>;
	readonly paramNames: ReadonlyArray<string>;
	readonly terminalKind: TerminalKind;
};

type SegmentNode<Payload extends object> = {
	readonly depth: number;
	readonly staticChildren: Map<string, SegmentNode<Payload>>;
	dynamicChild: SegmentNode<Payload> | null;
	indexPattern: TreePattern<Payload> | null;
	routePattern: TreePattern<Payload> | null;
	splatPattern: TreePattern<Payload> | null;
	trailingRoutePattern: TreePattern<Payload> | null;
};

type Tree<Payload extends object> = {
	readonly kind: MatcherKind;
	readonly root: SegmentNode<Payload>;
	/** Flat patterns made only of static segments, by their spelling, for an exact lookup before the walk. */
	readonly staticPatterns: ReadonlyMap<string, TreePattern<Payload>>;
};

const flatTree: unique symbol = Symbol("primitives-ts/path-matcher/flat");
const nestedTree: unique symbol = Symbol("primitives-ts/path-matcher/nested");

export type FlatMatcher = { readonly [flatTree]: Tree<object> };
export type NestedMatcher = { readonly [nestedTree]: Tree<object> };
export type NestedValueMatcher<Value> = {
	readonly [nestedTree]: Tree<{ readonly value: Value }>;
};

type AncestorLink<Payload extends object> = {
	readonly pattern: TreePattern<Payload>;
	readonly previous: AncestorLink<Payload> | null;
};

type NormalizedInputPath = {
	readonly hasTrailingSlash: boolean;
	readonly path: string;
	readonly segments: ReadonlyArray<string>;
};

type SearchAlternative<Payload extends object> =
	| {
			readonly kind: "node";
			readonly node: SegmentNode<Payload>;
			readonly segmentIndex: number;
			readonly ancestors: AncestorLink<Payload> | null;
	  }
	| {
			readonly kind: "splat";
			readonly pattern: TreePattern<Payload>;
			readonly segmentIndex: number;
			readonly ancestors: AncestorLink<Payload> | null;
	  };

const EMPTY_FIELDS = {};
const EMPTY_STRING_ARRAY: ReadonlyArray<string> = [];

/**
 * A registered pattern that occupies the matching position of an earlier
 * registered pattern.
 */
export type DuplicatePosition<Pattern extends CompiledPattern> = {
	/** The pattern that was registered first at the position. */
	readonly earlierPattern: Pattern;
	/** The earlier pattern's index among the registrations that the constructor received, counted from zero. */
	readonly earlierIndex: number;
	/** The pattern that was registered later at the same position. */
	readonly laterPattern: Pattern;
	/** The later pattern's index among the registrations that the constructor received, counted from zero. */
	readonly laterIndex: number;
};

/**
 * Registered patterns occupy the matching positions of earlier registered
 * patterns, so each of them matches the same paths as an earlier pattern and
 * a matcher could select only one of the two.
 */
export type DuplicatePositionError<Pattern extends CompiledPattern> = {
	readonly kind: "duplicate-position";
	/** Each pattern that occupies the position of an earlier pattern, in registration order. */
	readonly duplicates: readonly [
		DuplicatePosition<Pattern>,
		...Array<DuplicatePosition<Pattern>>,
	];
};

/**
 * Builds a matcher over flat patterns, or returns every pattern that occupies
 * the position of an earlier pattern, as the module documentation describes.
 * The matcher does not check its input otherwise. Each compiled pattern must
 * come from compileFlatPattern, directly or through JSON, or from the
 * serialized output of the Rust `primitives_rs::path_matcher` compiler, which
 * has the same shape. Other data, such as a splat segment that is not last,
 * gives unspecified matches.
 */
export function createFlatMatcher(
	patterns: ReadonlyArray<CompiledFlatPattern>,
): Result<FlatMatcher, DuplicatePositionError<CompiledFlatPattern>> {
	const tree = createTree(
		"flat",
		patterns,
		(pattern) => pattern,
		() => EMPTY_FIELDS,
	);
	return tree.kind === "error"
		? tree
		: createSuccessResult({ [flatTree]: tree.value });
}

/**
 * Builds a matcher over nested patterns, or returns every pattern that
 * occupies the position of an earlier pattern, as the module documentation
 * describes. Compiled patterns have the same requirements as in
 * createFlatMatcher; each must come from compileNestedPattern or from the
 * Rust compiler's nested patterns.
 */
export function createNestedMatcher(
	patterns: ReadonlyArray<CompiledNestedPattern>,
): Result<NestedMatcher, DuplicatePositionError<CompiledNestedPattern>> {
	const tree = createTree(
		"nested",
		patterns,
		(pattern) => pattern,
		() => EMPTY_FIELDS,
	);
	return tree.kind === "error"
		? tree
		: createSuccessResult({ [nestedTree]: tree.value });
}

/**
 * Retains each value with its pattern. Duplicate positions and compiled
 * patterns have the same rules as in createNestedMatcher.
 */
export function createNestedValueMatcher<Value>(
	registrations: ReadonlyArray<{
		readonly pattern: CompiledNestedPattern;
		readonly value: Value;
	}>,
): Result<
	NestedValueMatcher<Value>,
	DuplicatePositionError<CompiledNestedPattern>
> {
	const tree = createTree(
		"nested",
		registrations,
		(entry) => entry.pattern,
		(entry) => {
			return { value: entry.value };
		},
	);
	return tree.kind === "error"
		? tree
		: createSuccessResult({ [nestedTree]: tree.value });
}

/**
 * Finds one complete flat match or returns undefined. Matching trims JavaScript
 * whitespace around the path, inserts a missing leading slash, and collapses
 * repeated slashes. It does not parse URLs, percent-decode segments, resolve dot
 * segments, or change case. Static segments precede dynamic segments and splats;
 * a preferred branch that cannot complete permits a less-specific alternative.
 * A trailing-slash terminal takes priority when the path has a trailing slash.
 */
export function findBestMatch(
	matcher: FlatMatcher,
	inputPath: string,
): FlatMatch | undefined {
	return matchFlat(matcher[flatTree], inputPath);
}

/**
 * Finds the complete winning chain from outermost registered ancestor to leaf.
 * Path normalization follows findBestMatch and also ignores a trailing slash.
 * Each match contains only its own named params; only a splat leaf receives
 * splat values. An index terminal wins at its parent path. A missing complete
 * match returns undefined rather than a partial ancestor chain.
 */
export function findNestedMatches(
	matcher: NestedMatcher,
	inputPath: string,
): NestedMatches | undefined {
	return matchNested(matcher[nestedTree], inputPath);
}

/** Returns the same ordered chain as findNestedMatches with each retained registration value. */
export function findNestedValueMatches<Value>(
	matcher: NestedValueMatcher<Value>,
	inputPath: string,
): ReadonlyArray<NestedMatch & { readonly value: Value }> | undefined {
	return matchNested(matcher[nestedTree], inputPath);
}

function matchFlat<Payload extends object>(
	tree: Tree<Payload>,
	inputPath: string,
): (FlatMatch & Payload) | undefined {
	const exact = findExactStaticMatch(tree, inputPath);
	if (exact !== null) {
		return exact;
	}
	const normalized = normalizeInputPath({ inputPath, kind: "flat" });
	const normalizedExact = findExactStaticMatch(tree, normalized.path);
	if (normalizedExact !== null) {
		return normalizedExact;
	}
	const found = findTreeMatch(tree, normalized);
	return found === null ? undefined : buildFlatMatch(found, normalized);
}

function matchNested<Payload extends object>(
	tree: Tree<Payload>,
	inputPath: string,
): ReadonlyArray<NestedMatch & Payload> | undefined {
	const normalized = normalizeInputPath({ inputPath, kind: "nested" });
	const found = findTreeMatch(tree, normalized);
	return found === null
		? undefined
		: buildNestedMatches(found, normalized.segments);
}

function createTree<
	Registration,
	Pattern extends CompiledFlatPattern | CompiledNestedPattern,
	Payload extends object,
>(
	kind: MatcherKind,
	registrations: ReadonlyArray<Registration>,
	getPatternFn: (registration: Registration) => Pattern,
	getPayloadFn: (registration: Registration) => Payload,
): Result<Tree<Payload>, DuplicatePositionError<Pattern>> {
	const root = createSegmentNode<Payload>(0);
	const staticPatterns = new Map<string, TreePattern<Payload>>();
	const duplicates: Array<DuplicatePosition<Pattern>> = [];
	for (const [registrationIndex, registration] of registrations.entries()) {
		const compiled = getPatternFn(registration);
		const pattern = toTreePattern({
			compiled,
			payload: getPayloadFn(registration),
			registrationIndex,
		});
		let node = root;
		let isStatic = true;
		for (const segment of compiled.segments) {
			if (segment.kind === "static") {
				let child = node.staticChildren.get(segment.value);
				if (child === undefined) {
					child = createSegmentNode(node.depth + 1);
					node.staticChildren.set(segment.value, child);
				}
				node = child;
			} else if (segment.kind === "dynamic") {
				isStatic = false;
				if (node.dynamicChild === null) {
					node.dynamicChild = createSegmentNode(node.depth + 1);
				}
				node = node.dynamicChild;
			}
		}
		const earlier = readTerminalPattern(node, pattern.terminalKind);
		if (earlier !== null) {
			duplicates.push({
				earlierPattern: getPatternFn(
					registrations[earlier.registrationIndex]!,
				),
				earlierIndex: earlier.registrationIndex,
				laterPattern: compiled,
				laterIndex: registrationIndex,
			});
			continue;
		}
		switch (pattern.terminalKind) {
			case "route":
				node.routePattern = pattern;
				break;
			case "trailing-route":
				node.trailingRoutePattern = pattern;
				break;
			case "splat":
				node.splatPattern = pattern;
				break;
			case "index":
				node.indexPattern = pattern;
		}
		if (kind === "flat" && isStatic && pattern.terminalKind !== "splat") {
			staticPatterns.set(pattern.pattern, pattern);
		}
	}
	const [firstDuplicate, ...otherDuplicates] = duplicates;
	return firstDuplicate === undefined
		? createSuccessResult({ kind, root, staticPatterns })
		: createErrorResult({
				kind: "duplicate-position",
				duplicates: [firstDuplicate, ...otherDuplicates],
			});
}

/** The pattern that ends at `node` with this terminal kind, if one does. */
function readTerminalPattern<Payload extends object>(
	node: SegmentNode<Payload>,
	terminalKind: TerminalKind,
): TreePattern<Payload> | null {
	switch (terminalKind) {
		case "route":
			return node.routePattern;
		case "trailing-route":
			return node.trailingRoutePattern;
		case "splat":
			return node.splatPattern;
		case "index":
			return node.indexPattern;
	}
}

function toTreePattern<Payload extends object>(input: {
	readonly compiled: CompiledFlatPattern | CompiledNestedPattern;
	readonly payload: Payload;
	readonly registrationIndex: number;
}): TreePattern<Payload> {
	const { compiled } = input;
	const paramIndexes: Array<number> = [];
	const paramNames: Array<string> = [];
	let consumedSegmentCount = 0;
	let terminalKind: TerminalKind =
		compiled.kind === "flat" && compiled.hasTrailingSlash
			? "trailing-route"
			: "route";
	for (const segment of compiled.segments) {
		if (segment.kind === "dynamic") {
			paramIndexes.push(consumedSegmentCount);
			paramNames.push(segment.paramName);
			consumedSegmentCount++;
		} else if (segment.kind === "static") {
			consumedSegmentCount++;
		} else {
			terminalKind = segment.kind;
		}
	}
	return {
		payload: input.payload,
		registrationIndex: input.registrationIndex,
		pattern: compiled.pattern,
		consumedSegmentCount,
		paramIndexes,
		paramNames,
		terminalKind,
	};
}

function createSegmentNode<Payload extends object>(
	depth: number,
): SegmentNode<Payload> {
	return {
		depth,
		dynamicChild: null,
		indexPattern: null,
		routePattern: null,
		splatPattern: null,
		staticChildren: new Map(),
		trailingRoutePattern: null,
	};
}

/** A flat path spelled exactly like a static pattern, or like one plus a trailing slash. */
function findExactStaticMatch<Payload extends object>(
	tree: Tree<Payload>,
	path: string,
): (FlatMatch & Payload) | null {
	const exact = tree.staticPatterns.get(path);
	if (exact !== undefined) {
		return {
			...exact.payload,
			pattern: exact.pattern,
			dynamicParams: Object.create(null),
			splatValues: EMPTY_STRING_ARRAY,
			hasTrailingSlash: path.length > 1 && path.endsWith("/"),
		};
	}
	if (path.length > 1 && path.endsWith("/") && !path.includes("//")) {
		const withoutSlash = tree.staticPatterns.get(path.slice(0, -1));
		if (withoutSlash !== undefined) {
			return {
				...withoutSlash.payload,
				pattern: withoutSlash.pattern,
				dynamicParams: Object.create(null),
				splatValues: EMPTY_STRING_ARRAY,
				hasTrailingSlash: true,
			};
		}
	}
	return null;
}

type TreeMatch<Payload extends object> = {
	readonly ancestors: AncestorLink<Payload> | null;
	readonly pattern: TreePattern<Payload>;
	/** Records the index of the first segment that a splat consumed, or the segment count when the splat consumed no segments. */
	readonly splatStart: number;
};

function findTreeMatch<Payload extends object>(
	tree: Tree<Payload>,
	normalizedInput: NormalizedInputPath,
): TreeMatch<Payload> | null {
	let node = tree.root;
	let segmentIndex = 0;
	let ancestors: AncestorLink<Payload> | null = null;
	const alternatives: Array<SearchAlternative<Payload>> = [];

	for (;;) {
		if (segmentIndex === normalizedInput.segments.length) {
			const endMatch = findEndMatch({
				ancestors,
				hasTrailingSlash: normalizedInput.hasTrailingSlash,
				kind: tree.kind,
				node,
				segmentCount: normalizedInput.segments.length,
			});
			if (endMatch !== null) {
				return endMatch;
			}
		} else {
			const descendantAncestors: AncestorLink<Payload> | null =
				extendAncestorChain({
					ancestors,
					kind: tree.kind,
					node,
				});
			const inputSegment = normalizedInput.segments[segmentIndex]!;
			const staticChild = node.staticChildren.get(inputSegment);
			const dynamicChild = node.dynamicChild;
			const splatPattern = node.splatPattern;

			if (
				staticChild === undefined &&
				dynamicChild === null &&
				splatPattern !== null
			) {
				return {
					ancestors: descendantAncestors,
					pattern: splatPattern,
					splatStart: segmentIndex,
				};
			}

			if (splatPattern !== null) {
				alternatives.push({
					kind: "splat",
					pattern: splatPattern,
					segmentIndex,
					ancestors: descendantAncestors,
				});
			}
			if (staticChild !== undefined) {
				if (dynamicChild !== null) {
					alternatives.push({
						kind: "node",
						node: dynamicChild,
						segmentIndex: segmentIndex + 1,
						ancestors: descendantAncestors,
					});
				}
				node = staticChild;
				segmentIndex++;
				ancestors = descendantAncestors;
				continue;
			}
			if (dynamicChild !== null) {
				node = dynamicChild;
				segmentIndex++;
				ancestors = descendantAncestors;
				continue;
			}
		}

		const alternative = alternatives.pop();
		if (alternative === undefined) {
			return null;
		}
		if (alternative.kind === "splat") {
			return {
				ancestors: alternative.ancestors,
				pattern: alternative.pattern,
				splatStart: alternative.segmentIndex,
			};
		}
		node = alternative.node;
		segmentIndex = alternative.segmentIndex;
		ancestors = alternative.ancestors;
	}
}

function findEndMatch<Payload extends object>(input: {
	readonly ancestors: AncestorLink<Payload> | null;
	readonly hasTrailingSlash: boolean;
	readonly kind: MatcherKind;
	readonly node: SegmentNode<Payload>;
	readonly segmentCount: number;
}): TreeMatch<Payload> | null {
	const { node, segmentCount } = input;
	if (input.kind === "flat") {
		if (input.hasTrailingSlash && node.trailingRoutePattern !== null) {
			return {
				ancestors: null,
				pattern: node.trailingRoutePattern,
				splatStart: segmentCount,
			};
		}
		if (node.routePattern !== null) {
			return {
				ancestors: null,
				pattern: node.routePattern,
				splatStart: segmentCount,
			};
		}
		if (
			node.splatPattern !== null &&
			(node.depth === 0 || input.hasTrailingSlash)
		) {
			return {
				ancestors: null,
				pattern: node.splatPattern,
				splatStart: segmentCount,
			};
		}
		return null;
	}

	const descendantAncestors = extendAncestorChain(input);
	if (node.indexPattern !== null) {
		return {
			ancestors: descendantAncestors,
			pattern: node.indexPattern,
			splatStart: segmentCount,
		};
	}
	if (node.depth === 0 && node.splatPattern !== null) {
		return {
			ancestors: descendantAncestors,
			pattern: node.splatPattern,
			splatStart: segmentCount,
		};
	}
	if (node.routePattern !== null) {
		return {
			ancestors: input.ancestors,
			pattern: node.routePattern,
			splatStart: segmentCount,
		};
	}
	return null;
}

/** The ancestors of anything below `node`: its route pattern joins the chain in a nested tree. */
function extendAncestorChain<Payload extends object>(input: {
	readonly ancestors: AncestorLink<Payload> | null;
	readonly kind: MatcherKind;
	readonly node: SegmentNode<Payload>;
}): AncestorLink<Payload> | null {
	if (input.kind === "flat" || input.node.routePattern === null) {
		return input.ancestors;
	}
	return { pattern: input.node.routePattern, previous: input.ancestors };
}

function buildFlatMatch<Payload extends object>(
	match: TreeMatch<Payload>,
	normalizedInput: NormalizedInputPath,
): FlatMatch & Payload {
	return {
		...match.pattern.payload,
		pattern: match.pattern.pattern,
		dynamicParams: readDynamicParams(
			match.pattern,
			normalizedInput.segments,
		),
		splatValues: readSplatValues(
			match.pattern,
			normalizedInput.segments,
			match.splatStart,
		),
		hasTrailingSlash: normalizedInput.hasTrailingSlash,
	};
}

function buildNestedMatches<Payload extends object>(
	match: TreeMatch<Payload>,
	segments: ReadonlyArray<string>,
): ReadonlyArray<NestedMatch & Payload> {
	const positions: Array<NestedMatch & Payload> = [
		{
			...match.pattern.payload,
			pattern: match.pattern.pattern,
			dynamicParams: readDynamicParams(match.pattern, segments),
			splatValues: readSplatValues(
				match.pattern,
				segments,
				match.splatStart,
			),
		},
	];
	let ancestor = match.ancestors;
	while (ancestor !== null) {
		positions.push({
			...ancestor.pattern.payload,
			pattern: ancestor.pattern.pattern,
			dynamicParams: readDynamicParams(ancestor.pattern, segments),
			splatValues: EMPTY_STRING_ARRAY,
		});
		ancestor = ancestor.previous;
	}
	positions.reverse();
	return positions;
}

/** The pattern's named params from the path's segments; a null-prototype record, so any name is an own property. */
function readDynamicParams<Payload extends object>(
	pattern: TreePattern<Payload>,
	segments: ReadonlyArray<string>,
): DynamicParams {
	const dynamicParams: Record<string, string> = Object.create(null);
	for (const [index, name] of pattern.paramNames.entries()) {
		dynamicParams[name] = segments[pattern.paramIndexes[index]!]!;
	}
	return dynamicParams;
}

function readSplatValues<Payload extends object>(
	pattern: TreePattern<Payload>,
	segments: ReadonlyArray<string>,
	splatStart: number,
): ReadonlyArray<string> {
	return pattern.terminalKind === "splat" && splatStart < segments.length
		? segments.slice(splatStart)
		: EMPTY_STRING_ARRAY;
}

function normalizeInputPath(input: {
	readonly inputPath: string;
	readonly kind: MatcherKind;
}): NormalizedInputPath {
	let startIndex = 0;
	let endIndex = input.inputPath.length;
	while (
		startIndex < endIndex &&
		testJsWhitespaceCodeUnit(input.inputPath.charCodeAt(startIndex))
	) {
		startIndex++;
	}
	while (
		endIndex > startIndex &&
		testJsWhitespaceCodeUnit(input.inputPath.charCodeAt(endIndex - 1))
	) {
		endIndex--;
	}
	let normalizedPath =
		startIndex === 0 && endIndex === input.inputPath.length
			? input.inputPath
			: input.inputPath.slice(startIndex, endIndex);
	if (!normalizedPath.startsWith("/")) {
		normalizedPath = "/" + normalizedPath;
	}

	const segments: Array<string> = [];
	let segmentStart = 1;
	let hasRepeatedSlash = false;
	for (let index = 1; index < normalizedPath.length; index++) {
		if (normalizedPath[index] !== "/") {
			continue;
		}
		if (index === segmentStart) {
			hasRepeatedSlash = true;
		} else {
			segments.push(normalizedPath.slice(segmentStart, index));
		}
		segmentStart = index + 1;
	}
	if (segmentStart < normalizedPath.length) {
		segments.push(normalizedPath.slice(segmentStart));
	}
	const rawTrailingSlash =
		normalizedPath.length > 1 && normalizedPath.endsWith("/");
	if (hasRepeatedSlash) {
		normalizedPath =
			segments.length === 0
				? "/"
				: `/${segments.join("/")}${rawTrailingSlash ? "/" : ""}`;
	}
	if (
		input.kind === "nested" &&
		normalizedPath.length > 1 &&
		normalizedPath.endsWith("/")
	) {
		normalizedPath = normalizedPath.slice(0, -1);
	}

	const hasTrailingSlash =
		input.kind === "flat" &&
		normalizedPath.length > 1 &&
		normalizedPath.endsWith("/");
	return {
		hasTrailingSlash,
		path: normalizedPath,
		segments,
	};
}

function testJsWhitespaceCodeUnit(codeUnit: number): boolean {
	return (
		(codeUnit >= 0x0009 && codeUnit <= 0x000d) ||
		codeUnit === 0x0020 ||
		codeUnit === 0x00a0 ||
		codeUnit === 0x1680 ||
		(codeUnit >= 0x2000 && codeUnit <= 0x200a) ||
		codeUnit === 0x2028 ||
		codeUnit === 0x2029 ||
		codeUnit === 0x202f ||
		codeUnit === 0x205f ||
		codeUnit === 0x3000 ||
		codeUnit === 0xfeff
	);
}
