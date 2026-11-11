import {
	createNestedValueMatcher,
	findNestedValueMatches,
	type CompiledNestedPattern,
	type DuplicatePositionError,
} from "primitives-ts/path-matcher";
import { createSuccessResult, type Result } from "primitives-ts/results";
import { decodeMatch } from "../protocol/path.ts";

/*
What a client loader's early start needs: the matcher over the registrations
that the server has sent. It applies the matching rules shared with the Rust server,
over the server-supplied compiled patterns and canonical paths. The server's
tree holds a superset of these patterns, and a complete match here takes the
static branch at every depth that the server's tree also takes, which is what
the client loader registry's proof rests on. The client imports this module
after the first page commits, so none of it is in the entry chunk, and a client
that never receives it starts nothing early.
*/

type LearnedMatch<Value> = {
	readonly value: Value;
	readonly params: Readonly<Record<string, string>>;
	readonly splatValues: ReadonlyArray<string>;
};

export type LearnedMatcher<Value> = {
	/** The registrations that a canonical path matches completely, outermost to innermost; null when it matches none or a param does not decode. */
	readonly matchPath: (
		path: string,
	) => ReadonlyArray<LearnedMatch<Value>> | null;
};

/** Returns the matcher, or the server's patterns that occupy one matching position. */
export function createLearnedMatcher<Value>(
	registrations: ReadonlyArray<{
		readonly pattern: CompiledNestedPattern;
		readonly value: Value;
	}>,
): Result<
	LearnedMatcher<Value>,
	DuplicatePositionError<CompiledNestedPattern>
> {
	const created = createNestedValueMatcher(registrations);
	if (created.kind === "error") {
		return created;
	}
	const matcher = created.value;
	return createSuccessResult({
		matchPath: (path) => {
			const matches = findNestedValueMatches(matcher, path);
			if (matches === undefined) {
				return null;
			}
			const chain: Array<LearnedMatch<Value>> = [];
			for (const match of matches) {
				const decodedMatch = decodeMatch(match);
				if (decodedMatch.kind === "error") {
					// The server refuses such a path; there is nothing to start early.
					return null;
				}
				chain.push({ value: match.value, ...decodedMatch.value });
			}
			return chain;
		},
	});
}
