import type {
	AnyApiEndpointCall,
	AnyPatternMatch,
	QueryApiEndpointCall,
	ApiEndpointDeclaredErrorOf,
	ApiEndpointOutputOf,
} from "./generated-types.ts";
import type { RedirectStatus } from "./protocol/constants.ts";
import type { CancelPageCommit } from "./views.ts";

/*
The public result, state, event, and option types. Once a client has been
created, asynchronous commands resolve to values with a `kind`. Calling a
command without a client violates the application lifecycle.
*/

declare const API_ENDPOINT_CALL_ID: unique symbol;

/** An opaque identity that Vorma assigns to one endpoint call. */
export type ApiEndpointCallId = string & {
	readonly [API_ENDPOINT_CALL_ID]: true;
};

// Pages and errors

/**
 * A page's failure at its first failing position. A server failure carries a
 * declared error, a rejection with its client message, or neither. A browser
 * failure carries the status and client message that the thrown value gave
 * (an object with a numeric `status` and a string `clientMsg`), or 500 and
 * "An unexpected error occurred.", and the thrown value as `cause`.
 */
export type PageError<DeclaredError = unknown> =
	| {
			readonly kind: "declared-error";
			readonly declaredError: DeclaredError;
			readonly status: number;
	  }
	| {
			readonly kind: "rejection";
			readonly clientMsg: string;
			readonly status: number;
	  }
	| { readonly kind: "undeclared-failure"; readonly status: number }
	| {
			readonly kind: "client-loader-failure";
			readonly pattern: string;
			readonly status: number;
			readonly clientMsg: string;
			readonly cause: unknown;
	  }
	| {
			readonly kind: "render-failure";
			/**
			 * The pattern of the failing position; undefined when the position
			 * has no matched view, as for an error component that fails below the
			 * page's last position.
			 */
			readonly pattern: string | undefined;
			readonly status: number;
			readonly clientMsg: string;
			readonly cause: unknown;
	  }
	| {
			readonly kind: "boot-failure";
			readonly status: number;
			readonly clientMsg: string;
			readonly cause: unknown;
	  };

export type CommitReason = "boot" | "nav" | "popstate" | "revalidation" | "hmr";

/** The committed page. */
export type PageState = {
	readonly href: string;
	/** Undefined when the entry has none. */
	readonly historyState: unknown;
	readonly matches: ReadonlyArray<AnyPatternMatch>;
	readonly error: PageError | undefined;
	readonly commitReason: CommitReason;
};

/** A page's identity without its data, as client loaders receive it. */
export type PatternMatchSummary = {
	readonly pattern: string;
	readonly params: Readonly<Record<string, string>>;
	readonly splatValues: ReadonlyArray<string>;
	readonly input: unknown;
};

// Navigation

export type NavSource = "navigate" | "popstate" | "redirect";

export type NavResult =
	| { readonly kind: "committed"; readonly href: string }
	| { readonly kind: "unchanged"; readonly href: string }
	| { readonly kind: "superseded" }
	/** A transition hook or `commitPageFn` cancelled the commit. */
	| { readonly kind: "cancelled" }
	/** Another origin, a refused build, or a response that is not Vorma's. */
	| { readonly kind: "document-nav"; readonly href: string }
	| { readonly kind: "invalid-target"; readonly msg: string }
	| { readonly kind: "invalid-history-state"; readonly msg: string }
	| { readonly kind: "client-disposed" }
	| {
			readonly kind: "failure";
			readonly stage: "request" | "modules" | "commit" | "document";
			readonly cause: unknown;
	  };

export type PrefetchResult =
	| { readonly kind: "ready" }
	| { readonly kind: "invalid-target"; readonly msg: string }
	/** The committed page, or the page being navigated to. */
	| { readonly kind: "not-needed" }
	/** The read failed, was not Vorma's, or redirected. */
	| { readonly kind: "failed"; readonly cause: unknown }
	/** The signal aborted, or a revalidation or invalidation retired it. */
	| { readonly kind: "superseded" }
	| { readonly kind: "taken-by-nav" }
	| { readonly kind: "client-disposed" };

export type RevalidationFailureReason = "page" | "query" | "build-skew";

export type RevalidationResult =
	| { readonly kind: "revalidated" }
	| { readonly kind: "cancelled" }
	| {
			readonly kind: "failure";
			readonly reason: RevalidationFailureReason;
			readonly cause: unknown;
	  }
	| { readonly kind: "client-disposed" };

// Endpoint calls

type ApiEndpointCallResultCommon = { readonly callId: ApiEndpointCallId };

type ResponseMeta = {
	readonly status: number;
	readonly headers: Headers;
};

export type ApiEndpointCallResult<
	Call extends AnyApiEndpointCall = AnyApiEndpointCall,
> =
	| (ApiEndpointCallResultCommon &
			ResponseMeta & {
				readonly kind: "success";
				readonly output: ApiEndpointOutputOf<Call>;
			})
	| (ApiEndpointCallResultCommon &
			ResponseMeta & {
				readonly kind: "declared-error";
				readonly declaredError: ApiEndpointDeclaredErrorOf<Call>;
			})
	| (ApiEndpointCallResultCommon &
			ResponseMeta & {
				readonly kind: "rejection";
				readonly clientMsg: string;
			})
	| (ApiEndpointCallResultCommon &
			ResponseMeta & {
				readonly kind: "undeclared-failure";
			})
	| (ApiEndpointCallResultCommon & {
			readonly kind: "redirect";
			readonly href: string;
			readonly redirectStatus: RedirectStatus | undefined;
			readonly nav: Promise<NavResult>;
	  })
	/**
	 * The caller's `signal` aborted, or `invalidateClientData()` retired the
	 * generation of an in-flight query call. A call that ends this way publishes
	 * nothing, and no answer from before the end reaches the caller.
	 */
	| (ApiEndpointCallResultCommon & { readonly kind: "aborted" })
	/** A later call with the same `supersedeKey` replaced this call. */
	| (ApiEndpointCallResultCommon & { readonly kind: "superseded" })
	| { readonly kind: "invalid-input"; readonly msg: string }
	| (ApiEndpointCallResultCommon & {
			readonly kind: "transport-failure";
			readonly msg: string;
			readonly cause: unknown;
	  })
	| { readonly kind: "client-disposed" };

/**
 * The result of a mutation. `revalidation` is present when the mutation
 * scheduled a revalidation: it is absent with `skipAutoRevalidate`, and for a
 * call that failed before it was sent.
 */
export type MutationResult<
	Call extends AnyApiEndpointCall = AnyApiEndpointCall,
> = ApiEndpointCallResult<Call> & {
	readonly revalidation?: Promise<RevalidationResult>;
};

// State

export type BuildSkew = {
	readonly clientBuildId: string;
	/** The build that the server named; undefined when the refusing response did not name one. */
	readonly serverBuildId: string | undefined;
	readonly requestedHref: string;
	readonly requestKind: RequestKind;
	readonly status: number;
};

export type RequestKind =
	| "nav"
	| "prefetch"
	| "revalidation"
	| "query"
	| "mutation";

export type ClientState = {
	readonly page: PageState;
	readonly nav:
		| {
				readonly href: string;
				readonly source: NavSource;
				readonly skipWorkIndicator: boolean;
		  }
		| undefined;
	readonly lastNavResult: NavResult | undefined;
	readonly revalidation: {
		/** A demand is outstanding. */
		readonly required: boolean;
		readonly phase: "scheduled" | "running" | undefined;
		readonly skipWorkIndicator: boolean;
		readonly lastResult: RevalidationResult | undefined;
	};
	/** Prefetches that are preparing; they never count toward the work indicator. */
	readonly prefetches: ReadonlyArray<{ readonly href: string }>;
	readonly apiEndpointCalls: ReadonlyArray<{
		readonly callId: ApiEndpointCallId;
		readonly kind: "query" | "mutation";
		readonly method: string;
		readonly pattern: string;
		/** Known once the request is built. */
		readonly href: string | undefined;
		readonly phase: "preparing" | "sent";
		readonly skipWorkIndicator: boolean;
	}>;
	readonly workIndicator: { readonly visible: boolean };
	readonly buildSkew: BuildSkew | undefined;
};

export type FailureSource =
	| "boot"
	| "nav"
	| "revalidation"
	| "client-loader"
	| "client-output-cleanup"
	| "pattern-matching"
	| "render"
	| "hmr"
	| "link"
	| "listener";

export type ClientEvent =
	| {
			readonly kind: "api-endpoint-call-settled";
			readonly callId: ApiEndpointCallId;
			readonly result: ApiEndpointCallResult | MutationResult;
	  }
	| {
			readonly kind: "failure-reported";
			readonly source: FailureSource;
			readonly cause: unknown;
	  };

// Options

/**
 * Prepares each request that Vorma sends, for example to add a header. The
 * returned request replaces the one that Vorma built. A page read never
 * follows an HTTP redirect itself, whatever redirect mode the returned request
 * names. Endpoint calls do not support conditional request headers, such as
 * `If-None-Match`: an endpoint's `304 Not Modified` answer to one is a
 * `transport-failure`.
 */
export type PrepareRequestFn = (ctx: {
	readonly request: Request;
	readonly requestKind: RequestKind;
	readonly callId: ApiEndpointCallId | undefined;
}) => Request | Promise<Request>;

/**
 * Decides how each page commit happens. It must call `commit` or return
 * `CancelPageCommit` before it settles: a function that settles without either
 * fails the operation at the stage `"commit"`. The operation is committed as
 * soon as `commit` has run, so the function can wait afterward, for example
 * for an animation to end.
 */
export type CommitPageFn = (ctx: {
	readonly reason: Exclude<CommitReason, "boot" | "hmr">;
	readonly current: PageState;
	readonly next: PageState;
	/** Writes history, applies the document, and swaps the page, synchronously. */
	readonly commit: () => void;
	/**
	 * Aborts when the operation ends: when a newer operation supersedes it, and
	 * soon after `commit` has run, because the operation is then complete. Work
	 * that continues after `commit`, such as waiting for an animation to end,
	 * must not depend on this signal.
	 */
	readonly signal: AbortSignal;
}) => void | CancelPageCommit | Promise<void | CancelPageCommit>;

export type WorkIndicatorOptions = {
	/** Must be finite and between zero and 2,147,483,647 milliseconds. */
	readonly startDelayMillis: number;
	/** Must be finite and between zero and 2,147,483,647 milliseconds. */
	readonly stopDelayMillis: number;
	readonly skipNavs?: boolean;
	readonly skipRevalidations?: boolean;
	readonly skipApiEndpointCalls?: boolean;
};

export type RevalidateOnWindowFocusOptions = {
	/**
	 * A window focus revalidates a page that was prepared at least this long
	 * ago. Must be finite and zero or more; boot fails otherwise.
	 */
	readonly staleTimeMillis: number;
	readonly skipWorkIndicator?: boolean;
};

/** The options that do not involve a renderer's components. */
export type CoreOptions = {
	readonly prepareRequestFn?: PrepareRequestFn;
	readonly commitPageFn?: CommitPageFn;
	readonly workIndicator?: WorkIndicatorOptions;
	readonly revalidateOnWindowFocus?: RevalidateOnWindowFocusOptions;
};

export type BootResult =
	| { readonly kind: "booted" }
	| {
			readonly kind: "failure";
			readonly msg: string;
			readonly cause: unknown;
	  };

/** The value that `createVormaClient` returns: it starts and stops the document's client. */
export type VormaClient = {
	readonly boot: () => Promise<BootResult>;
	readonly dispose: () => void;
};

/** The state of a watched query. */
export type WatchedQueryState = {
	/** The most recent settled result, kept while a newer request is in flight. */
	readonly result: ApiEndpointCallResult | undefined;
	readonly isFetching: boolean;
};

export type WatchedQuery = {
	readonly getState: () => WatchedQueryState;
	readonly subscribe: (
		listener: (state: WatchedQueryState) => void,
	) => () => void;
	/** A new call replaces the current one; `undefined` stops fetching until a call is set. */
	readonly setCall: (call: QueryApiEndpointCall | undefined) => void;
	readonly refetch: () => void;
	/** Vorma stops fetching, and revalidation stops waiting for this query. */
	readonly stop: () => void;
};
