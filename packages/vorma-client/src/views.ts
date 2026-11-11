import type { ResourceScope } from "primitives-ts/resource-scopes";
import type {
	ViewClientOutputOf,
	ViewInputOf,
	OrUndefined,
	PatternMatch,
	ViewServerLoaderPattern,
	ViewServerOutputOf,
	ViewPattern,
} from "./generated-types.ts";
import type {
	CommitReason,
	PageState,
	PatternMatchSummary,
} from "./results.ts";

/**
 * Return `CancelPageCommit` from `beforePageCommit` or `commitPageFn` to cancel
 * a page commit. The page swap does not happen, and the operation resolves to
 * `{ kind: "cancelled" }`.
 *
 * When a Back or Forward traversal is cancelled, the client retains the
 * departed page and moves the browser back to the departed history entry if the
 * entry distance is known. When a browser does not support the Navigation API
 * and the departed history entry and the arrived history entry belong to
 * different history epochs (for example, after a plain fragment link adds an
 * entry that Vorma did not write), the entry distance is unknown. In that case,
 * the client replaces the arrived history entry in place with the retained page
 * in a new history epoch, which preserves forward history entries while
 * overwriting the arrived history entry.
 */
export const CancelPageCommit: unique symbol = Symbol(
	"@vorma/client/CancelPageCommit",
);

export type CancelPageCommit = typeof CancelPageCommit;

/**
 * Vorma owns this scope. Its signal aborts and its synchronous cleanups run
 * after the final preparation, displayed page, or cached output releases it.
 * Cleanup registered after disposal runs immediately. Register cleanup as soon
 * as a resource is acquired, including after an asynchronous acquisition.
 * Vorma reports cleanup failures. While the client is active, those failures
 * also produce `client-output-cleanup` client events.
 * Cleanups run once in reverse registration order, and removing a registration
 * does not run it. Cleanup and error reporting are synchronous. The client
 * unmounts its renderer before disposing output resources. Revalidation retains
 * outgoing resources until replacement; cache expiry cannot dispose resources
 * that a displayed page still retains. Output with an owned scope preserves
 * identity rather than structural-equality reuse.
 *
 * A shared application service should retain its own lifetime; register release
 * of the loader's interest in that service rather than unconditional disposal
 * of the service. References retained outside Vorma do not extend this scope.
 */
export type ClientOutputScope = Pick<ResourceScope, "signal" | "addCleanup">;

/**
 * What a client loader receives. A client loader can start before the server
 * has confirmed the read, so it can run for a read that the server then
 * rejects, for example because of search input that the server refuses. The
 * outcome of such a loader is discarded.
 */
export type ClientLoaderCtx<Pattern extends ViewPattern = ViewPattern> = {
	readonly href: string;
	readonly historyState: unknown;
	readonly pattern: Pattern;
	readonly params: PatternMatch<Pattern>["params"];
	readonly splatValues: ReadonlyArray<string>;
	readonly input: ViewInputOf<Pattern>;
	readonly knownPatternMatches: ReadonlyArray<PatternMatchSummary>;
	readonly serverConfirmedPatternMatches: Promise<
		ReadonlyArray<PatternMatchSummary>
	>;
	/** Resolves when the server confirms this position; never resolves if the server does not. */
	readonly serverOutput: Promise<ViewServerOutputOf<Pattern>>;
	/** Resolves with the server output of another position, or `undefined` when the server has no such position. */
	readonly getServerOutput: <Target extends ViewServerLoaderPattern>(
		pattern: Target,
	) => Promise<OrUndefined<ViewServerOutputOf<Target>>>;
	/**
	 * Cancels preparation work. It also aborts after a successful commit, so it
	 * must not control a resource that the displayed page continues to use.
	 */
	readonly signal: AbortSignal;
	/** Owns resources that remain necessary while Vorma retains this loader's output. */
	readonly outputScope: ClientOutputScope;
};

export type BeforePageCommitCtx = {
	readonly reason: Exclude<CommitReason, "boot" | "hmr">;
	/** The view is in the current page only, the next page only, or both. */
	readonly presence: "leaving" | "arriving" | "staying";
	readonly current: PageState;
	readonly next: PageState;
	readonly signal: AbortSignal;
};

/**
 * The client calls `BeforePageCommitFn` before a page commit for each view in
 * the current page or the next page that defines `beforePageCommit`. Returning
 * `CancelPageCommit` cancels the page commit; for a cancelled Back or Forward
 * traversal, the client restores browser history for the retained page by
 * moving back when the entry distance is known or by replacing the arrived
 * history entry in place when the entry distance is unknown.
 */
export type BeforePageCommitFn = (
	ctx: BeforePageCommitCtx,
) => void | CancelPageCommit | Promise<void | CancelPageCommit>;

const VIEW_MARKER: unique symbol = Symbol("@vorma/client/View");

/**
 * A view as the core reads it. `Component` is the component type of the
 * application's renderer, which the core never inspects. A definition without
 * a component is a pass-through view.
 */
export type RendererNeutralViewDef<Component = unknown> = {
	readonly pattern: string;
	/** Absent for a pass-through view, which renders only its `Outlet`. */
	readonly component?: Component;
	readonly childErrorComponent?: Component;
	readonly clientLoader?: (ctx: ClientLoaderCtx) => Promise<unknown>;
	readonly beforePageCommit?: BeforePageCommitFn;
	readonly staleTimeMillis?: number;
	readonly rerunClientLoaderOnHmr?: boolean;
};

/** Marks a view definition so that a module's default export can be recognized as a view. */
export function brandRendererNeutralView<Def extends object>(def: Def): Def {
	Object.defineProperty(def, VIEW_MARKER, { value: true });
	return def;
}

export function checkRendererNeutralView(
	value: unknown,
): value is RendererNeutralViewDef {
	return (
		typeof value === "object" &&
		value !== null &&
		Object.hasOwn(value, VIEW_MARKER)
	);
}

/** The data that a view's component reads. */
export type RendererNeutralViewProps<Pattern extends ViewPattern, Outlet> = {
	readonly pattern: Pattern;
	readonly params: PatternMatch<Pattern>["params"];
	readonly splatValues: ReadonlyArray<string>;
	readonly input: ViewInputOf<Pattern>;
	readonly serverOutput: ViewServerOutputOf<Pattern>;
	readonly clientOutput: OrUndefined<ViewClientOutputOf<Pattern>>;
	readonly Outlet: Outlet;
};
