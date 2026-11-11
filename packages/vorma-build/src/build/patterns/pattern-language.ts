import { PATTERN_PROTOCOL } from "@vorma/client/unstable";
import type * as Matcher from "primitives-ts/path-matcher";

/** The view pattern language, with the markers that the client and the server use. */
export const NESTED_PATTERN_LANGUAGE: Matcher.NestedPatternLanguage = {
	kind: "nested",
	dynamicParamPrefix: PATTERN_PROTOCOL.dynamicParamPrefix,
	explicitIndexSegment: PATTERN_PROTOCOL.indexSegment,
	splatSegment: PATTERN_PROTOCOL.splatSegment,
};
