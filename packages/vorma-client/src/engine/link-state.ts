import { callWithResult } from "primitives-ts/results";
import type {
	RawTarget,
	NavTarget,
	PatternMatch,
	ViewPattern,
} from "../generated-types.ts";
import { canonicalizeRequestPath } from "../protocol/path.ts";
import type { ClientState } from "../results.ts";
import { resolveTargetHref } from "./targets.ts";

export type LinkStateRules = {
	/** Reports neither active nor pending. */
	readonly skipMatching?: boolean;
	readonly includeSearch?: boolean;
	readonly includeHash?: boolean;
};

export type LinkState = {
	readonly active: { readonly exact: boolean; readonly ancestor: boolean };
	readonly pending: { readonly exact: boolean; readonly ancestor: boolean };
};

const NO_MATCH: LinkState = {
	active: { exact: false, ancestor: false },
	pending: { exact: false, ancestor: false },
};

type ParsedHref = { readonly url: URL; readonly path: string | null };

function parseHref(href: string): ParsedHref {
	const url = new URL(href, window.location.href);
	const path = canonicalizeRequestPath(url.pathname);
	return { url, path: path.kind === "ok" ? path.value : null };
}

function checkExactMatch(input: {
	readonly target: ParsedHref;
	readonly compared: ParsedHref;
	readonly rules: LinkStateRules;
}): boolean {
	const { target, compared, rules } = input;
	return (
		target.path !== null &&
		target.path === compared.path &&
		(rules.includeSearch !== true ||
			target.url.search === compared.url.search) &&
		(rules.includeHash !== true || target.url.hash === compared.url.hash)
	);
}

function checkAncestorMatch(input: {
	readonly target: ParsedHref;
	readonly compared: ParsedHref;
	readonly targetPattern: string | undefined;
	readonly comparedPatterns: ReadonlyArray<string> | undefined;
}): boolean {
	const { target, compared, targetPattern, comparedPatterns } = input;
	if (target.path === null || compared.path === null) {
		return false;
	}
	const prefix = target.path === "/" ? "/" : `${target.path}/`;
	return (
		target.path !== compared.path &&
		(prefix === "/" || compared.path.startsWith(prefix)) &&
		(targetPattern === undefined ||
			comparedPatterns === undefined ||
			comparedPatterns.includes(targetPattern))
	);
}

/**
 * The parts of the client state that a link's state reads: the committed
 * page's href and patterns, and the pending navigation's href.
 */
export type LinkStateFacts = {
	readonly committedHref: string;
	readonly committedPatterns: ReadonlyArray<string>;
	readonly pendingHref: string | undefined;
};

const linkStateFactsByState = new WeakMap<ClientState, LinkStateFacts>();

/** Selects the link state facts of a client state; every caller receives the same object for the same state. */
export function selectLinkStateFacts(state: ClientState): LinkStateFacts {
	const known = linkStateFactsByState.get(state);
	if (known !== undefined) {
		return known;
	}
	const facts: LinkStateFacts = {
		committedHref: state.page.href,
		committedPatterns: state.page.matches.map((match) => match.pattern),
		pendingHref: state.nav?.href,
	};
	linkStateFactsByState.set(state, facts);
	return facts;
}

/**
 * The active and pending facts of an href under its rules. An href on another
 * origin, an href that does not parse, and rules that skip matching are
 * neither active nor pending. `targetPattern` is the target's pattern, which
 * an ancestor match requires among the committed page's patterns.
 */
export function computeLinkState(input: {
	readonly href: string;
	readonly targetPattern: string | undefined;
	readonly facts: LinkStateFacts;
	readonly rules: LinkStateRules | undefined;
}): LinkState {
	const { facts, rules } = input;
	if (rules?.skipMatching === true) {
		return NO_MATCH;
	}
	const parsed = callWithResult(() => parseHref(input.href));
	if (
		parsed.kind === "error" ||
		parsed.value.url.origin !== window.location.origin
	) {
		return NO_MATCH;
	}
	const appliedRules: LinkStateRules = rules ?? {};
	const targetHref = parsed.value;
	const committed = callWithResult(() => parseHref(facts.committedHref));
	if (committed.kind === "error") {
		return NO_MATCH;
	}
	const pendingText = facts.pendingHref;
	const pending =
		pendingText === undefined
			? undefined
			: callWithResult(() => parseHref(pendingText));
	const pendingHref = pending?.kind === "ok" ? pending.value : undefined;
	return {
		active: {
			exact: checkExactMatch({
				target: targetHref,
				compared: committed.value,
				rules: appliedRules,
			}),
			ancestor: checkAncestorMatch({
				target: targetHref,
				compared: committed.value,
				targetPattern: input.targetPattern,
				comparedPatterns: facts.committedPatterns,
			}),
		},
		pending: {
			exact:
				pendingHref !== undefined &&
				checkExactMatch({
					target: targetHref,
					compared: pendingHref,
					rules: appliedRules,
				}),
			ancestor:
				pendingHref !== undefined &&
				checkAncestorMatch({
					target: targetHref,
					compared: pendingHref,
					targetPattern: input.targetPattern,
					comparedPatterns: undefined,
				}),
		},
	};
}

/**
 * The active and pending facts of a target under its rules. A target that
 * cannot be encoded, another origin, and a target with rules that skip matching are
 * neither active nor pending.
 */
export function getLinkState(
	state: ClientState,
	target: NavTarget,
	rules?: LinkStateRules,
): LinkState {
	const runtimeTarget = target as RawTarget;
	const resolved = resolveTargetHref(runtimeTarget);
	if (resolved.kind === "error") {
		return NO_MATCH;
	}
	return computeLinkState({
		href: resolved.value,
		targetPattern: runtimeTarget.pattern,
		facts: selectLinkStateFacts(state),
		rules,
	});
}

/** The match for a pattern in the committed page, with its exact types. */
export function getPatternMatch<Pattern extends ViewPattern>(
	state: ClientState,
	pattern: Pattern,
): PatternMatch<Pattern> | undefined {
	return state.page.matches.find((match) => match.pattern === pattern) as
		| PatternMatch<Pattern>
		| undefined;
}
