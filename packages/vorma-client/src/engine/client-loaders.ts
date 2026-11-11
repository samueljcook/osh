import { awaitWithAbort } from "primitives-ts/async-work/cancellation";
import type { CompiledNestedPattern } from "primitives-ts/path-matcher";
import { type ResourceScope } from "primitives-ts/resource-scopes";
import { callWithResult } from "primitives-ts/results";
import { canonicalizeRequestPath } from "../protocol/path.ts";
import { parseSearch, type SearchShape } from "../protocol/search.ts";
import type { RouteRecord } from "../protocol/wire.ts";
import type { PatternMatchSummary } from "../results.ts";
import type { ClientLoaderCtx, RendererNeutralViewDef } from "../views.ts";
import {
	createClientOutputResources,
	type ClientOutputResources,
} from "./client-output-resources.ts";
import type {
	createLearnedMatcher,
	LearnedMatcher,
} from "./learned-matching.ts";

/*
Client loaders can start before server confirmation for a recently confirmed
dynamic path or for the static prefix of a complete match against registrations
from visited pages. The registry contains only server-issued route records for
the loaded build and lasts for this document's lifetime. Early starts require
the matching module, which loads after the first page commits.
*/

const CONFIRMED_DYNAMIC_PATH_CAPACITY = 4_096;

type LearnedMatching = {
	readonly createLearnedMatcher: typeof createLearnedMatcher;
};

/** What a record taught the registry about one pattern; one entry per pattern for the document's life. */
type PatternFacts = {
	readonly pattern: string;
	readonly compiled: CompiledNestedPattern;
	/** Every segment is static or the index; a static match is the branch that the server's tree also takes. */
	readonly isStatic: boolean;
	readonly searchShape: SearchShape | undefined;
	view: RendererNeutralViewDef | undefined;
};

type ProvenMatch = {
	readonly pattern: string;
	readonly params: Readonly<Record<string, string>>;
	readonly splatValues: ReadonlyArray<string>;
	readonly facts: PatternFacts;
};

type EligibleMatch = {
	readonly match: PatternMatchSummary;
	readonly view: RendererNeutralViewDef | undefined;
};

export type ClientLoaderRegistry = {
	readonly replaceViews: (
		replacements: ReadonlyMap<unknown, unknown>,
	) => void;
	/** Records the chain that the server confirmed for a URL. */
	readonly recordConfirmedChain: (input: {
		readonly url: URL;
		readonly positions: ReadonlyArray<{
			readonly record: RouteRecord;
			readonly view: RendererNeutralViewDef | undefined;
		}>;
	}) => void;
	readonly installMatching: (matching: LearnedMatching) => void;
	/** The proven chain prefix for `url`, with each position's search input parsed but not validated. */
	readonly findEligibleMatches: (url: URL) => ReadonlyArray<EligibleMatch>;
};

export function createClientLoaderRegistry(
	reportMatchingFailure: (cause: unknown) => void,
): ClientLoaderRegistry {
	const facts = new Map<string, PatternFacts>();
	const confirmedPaths = new Map<string, ReadonlyArray<ProvenMatch>>();
	type MatchingState =
		| { readonly kind: "uninstalled" }
		| { readonly kind: "dirty"; readonly implementation: LearnedMatching }
		| {
				readonly kind: "ready";
				readonly implementation: LearnedMatching;
				readonly matcher: LearnedMatcher<PatternFacts>;
		  }
		/** The learned patterns occupy one matching position twice; a new pattern makes the state dirty again. */
		| {
				readonly kind: "unmatchable";
				readonly implementation: LearnedMatching;
		  };
	let matching: MatchingState = { kind: "uninstalled" };

	function invalidateMatcher(): void {
		if (matching.kind !== "uninstalled") {
			matching = {
				kind: "dirty",
				implementation: matching.implementation,
			};
		}
	}

	function getMatcher(): LearnedMatcher<PatternFacts> | null {
		if (
			matching.kind === "uninstalled" ||
			matching.kind === "unmatchable"
		) {
			return null;
		}
		if (matching.kind === "dirty") {
			const created = matching.implementation.createLearnedMatcher(
				[...facts.values()].map((own) => {
					return {
						pattern: own.compiled,
						value: own,
					};
				}),
			);
			if (created.kind === "error") {
				matching = {
					kind: "unmatchable",
					implementation: matching.implementation,
				};
				reportMatchingFailure(created.error);
				return null;
			}
			matching = {
				kind: "ready",
				implementation: matching.implementation,
				matcher: created.value,
			};
		}
		return matching.matcher;
	}

	function matchLearned(path: string): ReadonlyArray<ProvenMatch> | null {
		const chain = getMatcher()?.matchPath(path) ?? null;
		if (chain === null) {
			return null;
		}
		return chain.map((match) => {
			return {
				pattern: match.value.pattern,
				params: match.params,
				splatValues: match.splatValues,
				facts: match.value,
			};
		});
	}

	function selectStaticPrefix(
		chain: ReadonlyArray<ProvenMatch>,
	): ReadonlyArray<ProvenMatch> {
		const end = chain.findIndex((match) => !match.facts.isStatic);
		return end === -1 ? chain : chain.slice(0, end);
	}

	/** The chain that the registry can prove for `path`: the confirmed chain while the learned patterns agree with it, else the static prefix of a learned match. */
	function proveChain(path: string): ReadonlyArray<ProvenMatch> {
		const learned = matchLearned(path);
		const confirmed = confirmedPaths.get(path);
		if (confirmed !== undefined) {
			const agrees =
				learned === null ||
				(learned.length === confirmed.length &&
					learned.every((match, index) => {
						return confirmed[index]?.pattern === match.pattern;
					}));
			if (agrees) {
				confirmedPaths.delete(path);
				confirmedPaths.set(path, confirmed);
				return confirmed;
			}
			confirmedPaths.delete(path);
		}
		return learned === null ? [] : selectStaticPrefix(learned);
	}

	function recordConfirmedPath(
		path: string,
		matches: ReadonlyArray<ProvenMatch>,
	): void {
		confirmedPaths.delete(path);
		const firstDynamicIndex = matches.findIndex(
			(match) => !match.facts.isStatic,
		);
		if (firstDynamicIndex === -1) {
			return;
		}
		const enablesAdditionalLoader = matches.some((match, index) => {
			return (
				index >= firstDynamicIndex &&
				match.facts.view?.clientLoader !== undefined
			);
		});
		if (!enablesAdditionalLoader) {
			return;
		}
		confirmedPaths.set(path, matches);
		if (confirmedPaths.size <= CONFIRMED_DYNAMIC_PATH_CAPACITY) {
			return;
		}
		for (const leastRecentlyUsedPath of confirmedPaths.keys()) {
			confirmedPaths.delete(leastRecentlyUsedPath);
			break;
		}
	}

	return {
		replaceViews: (replacements) => {
			for (const own of facts.values()) {
				if (own.view !== undefined && replacements.has(own.view)) {
					own.view = replacements.get(own.view) as
						| RendererNeutralViewDef
						| undefined;
				}
			}
		},
		recordConfirmedChain: ({ url, positions }) => {
			const proven: Array<ProvenMatch> = [];
			for (const { record, view } of positions) {
				let own = facts.get(record.pattern);
				if (own === undefined) {
					own = {
						pattern: record.pattern,
						compiled: record.compiledPattern,
						isStatic: record.compiledPattern.segments.every(
							(segment) => {
								return (
									segment.kind === "static" ||
									segment.kind === "index"
								);
							},
						),
						searchShape: record.searchShape,
						view,
					};
					facts.set(record.pattern, own);
					invalidateMatcher();
				}
				proven.push({
					pattern: record.pattern,
					params: record.params,
					splatValues: record.splatValues,
					facts: own,
				});
			}
			const path = canonicalizeRequestPath(url.pathname);
			if (path.kind === "ok") {
				recordConfirmedPath(path.value, proven);
			}
		},
		installMatching: (installed) => {
			matching = { kind: "dirty", implementation: installed };
		},
		findEligibleMatches: (url) => {
			if (matching.kind === "uninstalled") {
				return [];
			}
			const path = canonicalizeRequestPath(url.pathname);
			if (path.kind === "error") {
				return [];
			}
			const proved = callWithResult(() => proveChain(path.value));
			if (proved.kind === "error") {
				reportMatchingFailure(proved.error);
				return [];
			}
			const eligible: Array<EligibleMatch> = [];
			for (const match of proved.value) {
				let input: unknown;
				if (match.facts.searchShape !== undefined) {
					// The parser refuses input that the contract does not describe.
					const parsed = parseSearch({
						shape: match.facts.searchShape,
						params: url.searchParams,
					});
					if (parsed.kind === "error") {
						break;
					}
					input = parsed.value;
				}
				eligible.push({
					match: {
						pattern: match.pattern,
						params: match.params,
						splatValues: match.splatValues,
						input,
					},
					view: match.facts.view,
				});
			}
			return eligible;
		},
	};
}

/** The one server result that every client loader of a candidate selects from. */
type Confirmation = {
	readonly records: Promise<ReadonlyArray<RouteRecord>>;
	readonly confirm: (records: ReadonlyArray<RouteRecord>) => void;
};

export function createConfirmation(): Confirmation {
	const deferred = Promise.withResolvers<ReadonlyArray<RouteRecord>>();
	return { records: deferred.promise, confirm: deferred.resolve };
}

export function toKnownMatch(record: RouteRecord): PatternMatchSummary {
	return {
		pattern: record.pattern,
		params: record.params,
		splatValues: record.splatValues,
		input:
			record.searchInput.kind === "present"
				? record.searchInput.value
				: undefined,
	};
}

type LoaderOutcome =
	| { readonly kind: "success"; readonly value: unknown }
	| { readonly kind: "failure"; readonly cause: unknown }
	/** The loader's work was cancelled, so it says nothing about the page. */
	| { readonly kind: "no-result" };

export type RunningLoader = {
	readonly positionIndex: number;
	readonly pattern: string;
	readonly scope: ResourceScope;
	readonly outcome: Promise<LoaderOutcome>;
	readonly outputResources: ClientOutputResources;
};

/**
 * Starts one client loader, joined to its candidate's server confirmation. The
 * loader's scope is a child of `parentScope`; closing it aborts the loader's
 * signal and makes its outcome `no-result`.
 */
export function startClientLoader(input: {
	readonly view: RendererNeutralViewDef;
	readonly positionIndex: number;
	readonly href: string;
	readonly historyState: unknown;
	readonly match: PatternMatchSummary;
	readonly known: ReadonlyArray<PatternMatchSummary>;
	readonly confirmation: Confirmation;
	readonly parentScope: ResourceScope;
	readonly outputParentScope: ResourceScope;
}): RunningLoader {
	const { match, positionIndex } = input;
	const context = {
		href: input.href,
		historyState: input.historyState,
		pattern: match.pattern,
		params: match.params,
		splatValues: match.splatValues,
		input: match.input,
		knownPatternMatches: input.known,
		serverConfirmedPatternMatches: input.confirmation.records.then(
			(records) => records.map(toKnownMatch),
		),
		serverOutput: input.confirmation.records.then((records) => {
			const record = records[positionIndex];
			// A position that the server did not confirm is aborted by reconciliation.
			if (record?.pattern !== match.pattern) {
				return new Promise<never>(() => undefined);
			}
			return record.loaderOutput.kind === "present"
				? record.loaderOutput.value
				: undefined;
		}),
		getServerOutput: (pattern) => {
			return input.confirmation.records.then((records) => {
				const record = records.find((candidate) => {
					return candidate.pattern === pattern;
				});
				return record?.loaderOutput.kind === "present"
					? record.loaderOutput.value
					: undefined;
			});
		},
	} as Omit<ClientLoaderCtx, "signal" | "outputScope">;
	return invokeClientLoader({ ...input, context });
}

/** Invokes a loader with separate preparation and output lifetimes. */
export function invokeClientLoader(input: {
	readonly view: RendererNeutralViewDef;
	readonly positionIndex: number;
	readonly context: Omit<ClientLoaderCtx, "signal" | "outputScope">;
	readonly parentScope: ResourceScope;
	readonly outputParentScope: ResourceScope;
}): RunningLoader {
	const scope = input.parentScope.createChild();
	const outputResources = createClientOutputResources({
		preparationScope: scope,
		documentScope: input.outputParentScope,
	});
	const clientLoaderCtx: ClientLoaderCtx = {
		...input.context,
		signal: scope.signal,
		get outputScope() {
			return outputResources.outputScope;
		},
	};
	const { clientLoader } = input.view;
	// A loader that was cancelled and ignores its signal never delays the page.
	const outcome: Promise<LoaderOutcome> =
		clientLoader === undefined
			? Promise.resolve({ kind: "no-result" })
			: awaitWithAbort({
					signal: scope.signal,
					operationFn: () => clientLoader(clientLoaderCtx),
				}).then((result) => {
					if (result.kind === "ok") {
						return {
							kind: "success",
							value: result.value,
						} as const;
					}
					scope.dispose();
					return result.error.kind === "aborted"
						? ({ kind: "no-result" } as const)
						: ({
								kind: "failure",
								cause: result.error.cause,
							} as const);
				});
	return {
		positionIndex: input.positionIndex,
		pattern: input.context.pattern,
		scope,
		outcome,
		outputResources,
	};
}
