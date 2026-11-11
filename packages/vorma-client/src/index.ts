import { callWithResult } from "primitives-ts/results";
import { awaitBoot } from "./engine/client.ts";
import { requireCtx } from "./engine/current.ts";
import { callApiEndpointRaw } from "./engine/endpoints.ts";
import { subscribeToEvents } from "./engine/events.ts";
import {
	advanceGeneration,
	startNav,
	restartPendingNav,
} from "./engine/nav.ts";
import { prefetchDestination } from "./engine/prefetch.ts";
import { registerRevalidationDemand } from "./engine/revalidation.ts";
import { checkNavigableOrigin, resolveTargetHref } from "./engine/targets.ts";
import { watchQuery as watchQueryImplementation } from "./engine/watched-queries.ts";
import type {
	AnyApiEndpointCall,
	RawTarget,
	MutationApiEndpointCall,
	NavTarget,
	RejectUndeclaredOptions,
	QueryApiEndpointCall,
	RawApiEndpointCall,
	StaticPublicAssetName,
} from "./generated-types.ts";
import type {
	ClientEvent,
	ClientState,
	ApiEndpointCallResult,
	MutationResult,
	NavResult,
	PrefetchResult,
	RevalidationResult,
	WatchedQuery,
} from "./results.ts";

export type {
	AnyApiEndpointCall,
	AnyPatternMatch,
	ApiEndpointContracts,
	ApiEndpointDeclaredErrorOf,
	ApiEndpointOutputOf,
	ApiEndpointRequestOptions,
	HrefTarget,
	MutationApiEndpointCall,
	NavTarget,
	OrUndefined,
	PageTarget,
	ParamName,
	PatternMatch,
	QueryApiEndpointCall,
	RejectUndeclaredOptions,
	StaticPublicAssetName,
	ViewChildDeclaredErrorOf,
	ViewClientOutputOf,
	ViewContracts,
	ViewDeclaredErrorOf,
	ViewInputOf,
	ViewPattern,
	ViewSearchOf,
	ViewServerLoaderPattern,
	ViewServerOutputOf,
	VormaGeneratedTypes,
} from "./generated-types.ts";
export type {
	ApiEndpointCallId,
	ApiEndpointCallResult,
	BootResult,
	BuildSkew,
	ClientEvent,
	ClientState,
	CommitPageFn,
	CommitReason,
	CoreOptions,
	FailureSource,
	MutationResult,
	NavResult,
	NavSource,
	PageError,
	PageState,
	PatternMatchSummary,
	PrefetchResult,
	PrepareRequestFn,
	RequestKind,
	RevalidateOnWindowFocusOptions,
	RevalidationFailureReason,
	RevalidationResult,
	VormaClient,
	WatchedQuery,
	WatchedQueryState,
	WorkIndicatorOptions,
} from "./results.ts";
export type { RedirectStatus } from "./protocol/constants.ts";
export type {
	BeforePageCommitCtx,
	BeforePageCommitFn,
	ClientLoaderCtx,
	ClientOutputScope,
} from "./views.ts";
export { CancelPageCommit } from "./views.ts";
export type { LinkState, LinkStateRules } from "./engine/link-state.ts";
export { getLinkState, getPatternMatch } from "./engine/link-state.ts";
export type { UrlSync, UrlSyncOptions } from "./engine/url-sync.ts";
export { createUrlSync } from "./engine/url-sync.ts";
export { includeInWorkIndicator } from "./engine/work-indicator.ts";

/** Options of a navigation. */
export type NavOptions = {
	/** Replaces the history entry instead of adding one. */
	readonly replace?: boolean;
	/** A URL hash still scrolls to its target when false. */
	readonly scrollToTop?: boolean;
	/** False disables hash scrolling independently of scrolling to the top. */
	readonly scrollToHash?: boolean;
	/** Must be structured-cloneable. */
	readonly historyState?: unknown;
	readonly skipWorkIndicator?: boolean;
};

/**
 * Navigates to a page of the application or to an href. It resolves with the
 * outcome once a client has been created. Calling it without a client violates
 * the application lifecycle and rejects the promise.
 * Navigation waits until the destination's JavaScript modules have loaded and
 * until each stylesheet that Vorma adds for the destination has loaded or has
 * failed to load. A stylesheet that fails to load does not fail navigation.
 * The wait has no time limit: a stylesheet request that never settles keeps
 * the navigation pending until a newer navigation supersedes it, as a
 * browser's own document navigation waits for its stylesheets.
 * A redirect that Vorma's server makes for the destination, and that has no
 * fragment, inherits the original fragment; an explicit fragment replaces it,
 * and an empty `#` clears it. The client does not follow an HTTP redirect
 * that Vorma did not make: it loads the destination as a document, and the
 * browser follows that redirect. A document load writes history as the
 * navigation would: it adds an entry for a navigation that adds one, also
 * after the server redirected it; it replaces the current entry for
 * `replace`, for the redirect of an API endpoint call, for a redirect that a
 * revalidation receives, and for a redirect of a Back or Forward traversal;
 * and a Back or Forward traversal reloads the entry that the browser is on. Fragments remain browser-only and
 * do not enter HTTP requests. Application continuation query parameters retain
 * their ordinary meaning; Vorma does not interpret them. Client loaders receive
 * the complete destination URL through `href`. A target with a `javascript:`
 * href is an invalid target.
 *
 * A commit applies the destination's head. The managed head regions belong to
 * Vorma, which removes any other node that code inserts into them. A raw HTML
 * head item (the server's `add_head_dangerous_raw_html`) has a key that the
 * server computes, and the document holds it between the comments
 * `<!--vorma-raw:KEY-->` and `<!--/vorma-raw:KEY-->`. The two comments and
 * every node between them are one unit, including nodes that the item's own
 * scripts insert there, and deduplication does not apply to it. Items with
 * the same key are matched in order. A raw HTML item that both the committed
 * page and the destination have stays in the document, and its scripts do not
 * run again; a raw HTML item that the destination adds is inserted between
 * its two comments, and its scripts run; a raw HTML item that the destination
 * does not have is removed with its comments. This holds for the raw HTML of
 * the initial document too.
 *
 * Focus follows the scroll that a commit performs, as a browser's document
 * navigation sets it, and moving focus does not scroll. `scrollToTop` and
 * `scrollToHash` decide the scroll. A commit that scrolls to the top, or to
 * the position that a history traversal restores, resets focus to the
 * document: the active element becomes `body`, and sequential focus
 * navigation starts at the start of the document. A commit that scrolls to a
 * fragment target focuses the target when it can receive focus, and otherwise
 * makes it the starting point of sequential focus navigation. To make it the
 * starting point, Vorma gives it a `tabindex` of -1 for one focus and then
 * removes the attribute; an element that cannot receive focus even then, such
 * as one that is not rendered, leaves the starting point at the start of the
 * document. A commit that performs no scroll keeps focus where it is: a
 * revalidation, a URL synchronization that keeps the scroll position, a
 * navigation with `scrollToTop: false` and no fragment to scroll to, and a
 * navigation to a fragment that names no element and does not fall back to
 * the top. Boot keeps the focus that the browser gave the document. Vorma adds
 * no element that announces the navigation.
 */
export async function navigate(
	target: NavTarget & NavOptions,
): Promise<NavResult> {
	const ctx = requireCtx();
	const runtime = target as RawTarget & NavOptions;
	const href = resolveTargetHref(runtime);
	if (href.kind === "error") {
		return { kind: "invalid-target", msg: href.error };
	}
	const lifecycle = ctx.state.getValue().lifecycle;
	if (
		(lifecycle === "pre-boot" || lifecycle === "booting") &&
		!(await awaitBoot(ctx))
	) {
		return { kind: "client-disposed" };
	}
	return startNav(ctx, {
		href: href.value,
		replace: runtime.replace === true,
		historyState: runtime.historyState,
		scrollToTop: runtime.scrollToTop,
		scrollToHash: runtime.scrollToHash,
		skipWorkIndicator: runtime.skipWorkIndicator === true,
		source: "navigate",
		read: "shared",
	});
}

/**
 * Prepares a page so that a later navigation commits without waiting. It
 * resolves when the page is ready, and the signal cancels it and ends the life
 * of the page that it prepared.
 */
export async function prefetch(
	target: NavTarget & {
		readonly signal?: AbortSignal;
	},
): Promise<PrefetchResult> {
	const ctx = requireCtx();
	const runtime = target as RawTarget & { readonly signal?: AbortSignal };
	const href = resolveTargetHref(runtime);
	if (href.kind === "error") {
		return { kind: "invalid-target", msg: href.error };
	}
	const url = callWithResult(() => new URL(href.value, window.location.href));
	if (url.kind === "error" || !checkNavigableOrigin(url.value)) {
		return {
			kind: "invalid-target",
			msg: "A prefetch needs a page on this origin.",
		};
	}
	if (!(await awaitBoot(ctx))) {
		return { kind: "client-disposed" };
	}
	return prefetchDestination(ctx, { url: url.value, signal: runtime.signal });
}

/**
 * The href of a target, or why the target cannot be encoded. The query spells
 * the whole `input` object: every field that is not `undefined`, whether or
 * not a loader of the page declares it. An `input` that is not an object of
 * fields, such as a string, makes the target invalid, as a `javascript:` href
 * does.
 */
export function toHref(
	target: NavTarget,
):
	| { readonly kind: "href"; readonly href: string }
	| { readonly kind: "invalid-target"; readonly msg: string } {
	const href = resolveTargetHref(target as RawTarget);
	return href.kind === "ok"
		? { kind: "href", href: href.value }
		: { kind: "invalid-target", msg: href.error };
}

/**
 * Calls an endpoint. The call names the endpoint as its declaration does
 * (`kind`, `method`, and `pattern`), and the generated types reject any
 * combination that the server does not declare. A call takes only the options
 * of its own declaration: `skipAutoRevalidate` belongs to mutations alone, and
 * `input` appears only where the endpoint declares input. A mutation
 * revalidates the page afterward unless `skipAutoRevalidate` is set. A call made before the
 * first page commits, such as one from the first page's client loader, waits
 * only until boot has read the document's build ID, and it revalidates
 * nothing.
 */
export async function callApiEndpoint<const Call extends AnyApiEndpointCall>(
	call: Call & RejectUndeclaredOptions<Call>,
): Promise<
	Call extends MutationApiEndpointCall
		? MutationResult<Call>
		: ApiEndpointCallResult<Call>
> {
	const ctx = requireCtx();
	return (await callApiEndpointRaw(ctx, call as RawApiEndpointCall, {
		endsWithGeneration: true,
	})) as never;
}

/**
 * Watches a query endpoint: it fetches now, again when its call changes, and
 * again on every revalidation, until it stops. `setCall` compares the new
 * call with the watched call by contents, apart from `signal`. A call that is
 * the same query keeps the watched data and starts no fetch. The client uses
 * the signal of the latest call for every later fetch, including the fetches
 * of revalidations, so that signal must stay unaborted for as long as the
 * watcher should fetch: a fetch with an aborted signal ends at once and keeps
 * the earlier data. A signal with a deadline that each new call creates, such
 * as one from `AbortSignal.timeout`, must not be used.
 * Concurrent watchers share a request when the method, URL, body, request
 * options, caller signal, and work-indicator preference match. Stopping one
 * watcher cancels shared work only when no watcher remains. A supersede key
 * starts a separate call and cancels the preceding call with that key.
 */
export function watchQuery(
	call: QueryApiEndpointCall | undefined,
): WatchedQuery {
	return watchQueryImplementation(requireCtx(), call);
}

/** Reads the current page again and resolves when fresh data has committed. */
export async function revalidate(options?: {
	readonly skipWorkIndicator?: boolean;
}): Promise<RevalidationResult> {
	const ctx = requireCtx();
	if (!(await awaitBoot(ctx))) {
		return { kind: "client-disposed" };
	}
	return new Promise((resolve) => {
		registerRevalidationDemand(ctx, {
			immediate: false,
			skipWorkIndicator: options?.skipWorkIndicator === true,
			waiter: resolve,
		});
	});
}

/**
 * Retires the session's reads and reusable data, reads the pending destination
 * or the committed page and every watched query again, and resolves when they
 * are fresh.
 */
export async function invalidateClientData(): Promise<RevalidationResult> {
	const ctx = requireCtx();
	if (!(await awaitBoot(ctx))) {
		return { kind: "client-disposed" };
	}
	return new Promise((resolve) => {
		advanceGeneration(ctx);
		registerRevalidationDemand(ctx, {
			immediate: true,
			skipWorkIndicator: false,
			waiter: resolve,
		});
		restartPendingNav(ctx);
	});
}

/** The client's state, or undefined before the first page commits. */
export function getClientState(): ClientState | undefined {
	return requireCtx().publicState.getValue();
}

/**
 * Calls the listener after each change of the state. A listener that throws
 * never breaks Vorma or the other listeners.
 */
export function subscribeToClientState(
	listener: (state: ClientState, previous: ClientState | undefined) => void,
): () => void {
	return requireCtx().publicState.subscribe(
		({ value: state, previousValue: previous }) => {
			if (state !== undefined) {
				listener(state, previous);
			}
		},
	);
}

/** Calls the listener for each settled endpoint call and each failure that Vorma reports. */
export function subscribeToClientEvents(
	listener: (event: ClientEvent) => void,
): () => void {
	return subscribeToEvents(requireCtx(), listener);
}

/**
 * The hashed URL of a static public asset, by its name. The build replaces each lookup
 * that names an asset with a string literal; a lookup that it did not replace
 * throws. A lookup names its asset with a literal key, such as
 * `staticPublicAssets["logo.svg"]`, through this binding or an import alias, a
 * namespace import, a re-export, or a `const` alias of it; a computed key, or
 * the object passed elsewhere, is not a lookup that the build replaces, and a
 * literal type alone is not a literal key. Imports and re-exports may rename
 * the binding. A bundled module outside the client TypeScript project cannot
 * have lookups because the build cannot replace them. The build follows ESM
 * imports, re-exports, and constant aliases to report these reads. The build
 * cannot resolve arbitrary runtime value flow, including a read through a property
 * name that is a run-time value, such as `namespace[key]`, and that read throws
 * when it runs. CSS names a static public asset as
 * `url(@public/logo.svg)`, which the build replaces the same way. The static public asset source
 * directory publishes no entry named `.DS_Store` or `.env`, and none with a name that
 * starts with `.env.`, at any depth, so it has no asset name.
 * @vormaStaticPublicAsset The build identifies this binding through this declaration.
 */
export const staticPublicAssets: {
	readonly [Name in StaticPublicAssetName]: string;
} = new Proxy(
	{},
	{
		get: (_target, name) => {
			throw new Error(
				`staticPublicAssets[${JSON.stringify(String(name))}] was not replaced by the build.`,
			);
		},
	},
) as { readonly [Name in StaticPublicAssetName]: string };
