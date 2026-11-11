import { type DebouncedFunction } from "primitives-ts/async-work/debounce";
import type { ObservableValue } from "primitives-ts/observable-values";
import { type ResourceScope } from "primitives-ts/resource-scopes";
import type { HistoryPosition } from "../browser/history.ts";
import type { ScrollPosition } from "../browser/scroll.ts";
import type {
	ClientEvent,
	ClientState,
	CoreOptions,
	NavResult,
	NavSource,
	RevalidationResult,
} from "../results.ts";
import type { ClientLoaderRegistry } from "./client-loaders.ts";
import type { CoreState } from "./core-state.ts";
import type { PrefetchEntry } from "./prefetch.ts";
import type { Preparation, PreparedPage } from "./prepare.ts";

/** What the core needs from a renderer. */
export type RendererBinding = {
	/** Mounts the application's root with the renderer and returns the function that unmounts it. */
	readonly mountRoot: (rootElement: HTMLElement) => () => void;
	/** Runs `apply`, which updates the core's state, and returns after the renderer has updated the DOM for it. */
	readonly flushSync: (apply: () => void) => void;
};

export type RendererOwnership =
	| { readonly kind: "unmounted" }
	| { readonly kind: "mounting" }
	| { readonly kind: "mounted"; readonly unmountFn: () => void }
	| { readonly kind: "closing"; readonly finishFn: () => void }
	| { readonly kind: "closed" };

export type BootMetadata = {
	readonly clientBuildId: string;
	readonly deploymentId: string | undefined;
	readonly devMode: boolean;
};

export type Waiter = (result: RevalidationResult) => void;

export type FreshnessDemand = {
	readonly afterSequence: number;
	readonly skipWorkIndicator: boolean;
	readonly expectedQueries: ReadonlySet<string>;
	readonly acknowledgedQueries: ReadonlyMap<string, number>;
	readonly page:
		| { readonly kind: "awaiting" }
		| { readonly kind: "read"; readonly sequence: number };
	readonly waiters: ReadonlyArray<Waiter>;
};

/**
 * The candidate that executes a running attempt: a revalidation candidate that
 * reads the committed page, or the navigation in the slot, which settles the
 * attempt with its commit.
 */
export type AttemptExecutor = "revalidation" | "navigation";

export type FreshnessAttempt =
	| { readonly phase: "idle" }
	| {
			readonly phase: "running";
			readonly executor: AttemptExecutor;
			readonly coveredSequence: number;
			readonly renewal: "original" | "renewed";
			readonly waiters: ReadonlyArray<Waiter>;
	  }
	| { readonly phase: "failed"; readonly coveredSequence: number };

export type FreshnessFacts = {
	/** Every registered demand advances this quantitative ordering value. */
	readonly epoch: number;
	readonly state:
		| { readonly kind: "idle" }
		| {
				readonly kind: "outstanding";
				readonly demand: FreshnessDemand;
				readonly attempt: FreshnessAttempt;
		  };
};

export type NavIntent = {
	/** The absolute destination, including its hash. */
	readonly href: string;
	readonly historyState: unknown;
	readonly scrollToTop: boolean | undefined;
	readonly scrollToHash: boolean | undefined;
	readonly skipWorkIndicator: boolean;
};

/**
 * Where a history entry is: its Vorma position, and its index in the
 * Navigation API's entries where the browser has that API.
 */
export type EntryLocation = {
	readonly position: HistoryPosition | undefined;
	readonly navigationIndex: number | undefined;
};

/**
 * The history entry that a traversal left, recorded so that Vorma can
 * recognize its restoration `popstate` when a cancelled traversal steps back.
 */
export type DepartedEntryLocation = EntryLocation & {
	readonly entryKey: string | undefined;
};

/**
 * The history entry on which a traversal arrived, including its identity and
 * URL so that a cancelled traversal can verify that the browser has not moved
 * elsewhere before stepping back.
 */
export type ArrivedEntryLocation = EntryLocation & {
	readonly href: string;
	readonly entryKey: string | undefined;
};

/**
 * What a candidate is. The role decides how the commit writes history, how a
 * document load writes history, the reason that hooks receive, the public
 * navigation source, and what a cancel or a failure does. A redirect gives a
 * navigation a new role value.
 */
export type CandidateRole =
	/** A navigation that adds a history entry. */
	| { readonly kind: "push"; readonly source: "navigate" | "redirect" }
	/** A navigation that replaces the current history entry. */
	| { readonly kind: "replace"; readonly source: "navigate" | "redirect" }
	/**
	 * A Back or Forward traversal: the browser is already on the destination's
	 * entry, and the commit takes that entry. A cancel returns the browser to
	 * the departed entry, a failure reloads the document, and a document load
	 * reloads the entry.
	 */
	| {
			readonly kind: "traversal";
			readonly departed: EntryLocation;
			readonly arrived: ArrivedEntryLocation;
			/** The scroll position that the arrived entry remembered. */
			readonly restoreScroll: ScrollPosition | undefined;
	  }
	/**
	 * A traversal after the server redirected its read: the commit replaces the
	 * arrived entry with the redirect's destination. A cancel and a failure
	 * act as for a traversal.
	 */
	| {
			readonly kind: "redirected-traversal";
			readonly departed: EntryLocation;
			readonly arrived: ArrivedEntryLocation;
	  }
	/** A read of the committed page for a revalidation demand; the commit takes the browser's entry. */
	| { readonly kind: "revalidation" };

type CandidateCompletion =
	| {
			readonly kind: "pending";
			readonly observers: Array<(result: NavResult) => void>;
	  }
	| {
			readonly kind: "accepted";
			readonly result: NavResult;
			readonly observers: Array<(result: NavResult) => void>;
	  }
	| { readonly kind: "ended"; readonly result: NavResult };

type PreparationOwnership =
	| { readonly kind: "vacant" }
	| { readonly kind: "owned"; readonly preparation: Preparation };

/** One navigation or revalidation attempt in flight. At most one candidate occupies the slot. */
export type Candidate = {
	readonly scope: ResourceScope;
	/** A redirect replaces the role and the intent's href. */
	role: CandidateRole;
	intent: NavIntent;
	readonly generation: number;
	preparation: PreparationOwnership;
	/** Accepted outcomes survive later interruption; Ended owns no observers. */
	completion: CandidateCompletion;
};

/**
 * Explicit lifecycle of the navigation and history-return machine:
 * - `idle`: ready for a candidate or revalidation attempt.
 * - `running`: one candidate owns the navigation slot.
 * - `returning-history`: `history.go(distance)` was initiated for a cancelled
 *   traversal and awaits the restoration `popstate` (`initiating` while
 *   `history.go` is on the call stack, `awaiting-popstate` once it returns).
 * - `disposed`: the client was closed and accepts no further transitions.
 */
export type NavMachine =
	| { readonly kind: "idle" }
	| { readonly kind: "running"; readonly candidate: Candidate }
	| {
			readonly kind: "returning-history";
			readonly departed: DepartedEntryLocation;
			readonly phase: "initiating" | "awaiting-popstate";
	  }
	| { readonly kind: "disposed" };

export type RetainedOutputs = {
	readonly prepared: PreparedPage;
	readonly expiresAt: number;
	state:
		| {
				readonly kind: "retained";
				readonly timer: ReturnType<typeof setTimeout>;
				readonly releaseOutputs: () => void;
		  }
		| { readonly kind: "released" };
};

export type ClientCtx = {
	readonly options: CoreOptions;
	readonly renderer: RendererBinding;
	/** The application's lifetime. */
	readonly scope: ResourceScope;
	/** Output resources outlive preparation and remain available during renderer teardown. */
	readonly outputResourceScope: ResourceScope;
	releaseFrameOutputs: () => void;
	readonly state: ObservableValue<CoreState>;
	readonly publicState: ObservableValue<ClientState | undefined>;
	readonly eventListeners: Set<(event: ClientEvent) => void>;
	/** Called whenever a navigation of any source starts so that URL synchronization can cancel a pending target. */
	readonly navStartListeners: Set<(source: NavSource) => void>;
	readonly loaders: ClientLoaderRegistry;
	readonly retainedOutputs: Map<string, RetainedOutputs>;
	readonly prefetchEntries: Map<string, PrefetchEntry>;
	/**
	 * The document's build metadata. Waiters receive true when boot reads it,
	 * and false when boot fails before it reads it or the client is disposed
	 * first.
	 */
	bootMetadata:
		| {
				readonly kind: "unread";
				readonly waiters: Array<(read: boolean) => void>;
		  }
		| { readonly kind: "read"; readonly metadata: BootMetadata }
		| { readonly kind: "unavailable" };
	/** The last number that orders reads and demands; never the wall clock. */
	lastSequence: number;
	/** Advances to discard the late results of older reads. */
	generation: number;
	frameRevision: number;
	/** The renderer applies the scroll intent of the frame with the pending revision once. */
	scrollApplication:
		| { readonly kind: "unavailable" }
		| { readonly kind: "pending"; readonly revision: number }
		| { readonly kind: "applied"; readonly revision: number };
	navMachine: NavMachine;
	freshness: FreshnessFacts;
	revalidationScheduling:
		| { readonly kind: "uninitialized" }
		| {
				readonly kind: "ready";
				readonly debounce: DebouncedFunction<[], void>;
		  };
	rendererOwnership: RendererOwnership;
	/** The watched queries that are not stopped. */
	readonly watchedQueries: Set<QueryRuntime>;
	/** The endpoint calls that are unfinished, by supersede key. */
	readonly runsBySupersedeKey: Map<string, EndpointRun>;
	readonly endpointRuns: Set<EndpointRun>;
	/** The last skew that was reported, to report each once. */
	lastSkewKey: string | undefined;
};

/** A watched query's runtime; defined by `watched-queries.ts`. */
export type QueryRuntime = {
	readonly id: string;
	/** Fetches again for a demand and reports whether the query is expected to acknowledge. */
	readonly fetchForDemand: () => boolean;
	readonly stopWatching: () => void;
};

/**
 * Why an endpoint call ended before it settled: a later call with the same
 * supersede key, the caller's signal, the end of the generation that the call
 * belongs to, or the disposal of the client.
 */
export type EndpointInterruption =
	| "superseded"
	| "caller-aborted"
	| "generation-retired"
	| "disposed";

/** An unfinished endpoint call; defined by `endpoints.ts`. */
export type EndpointRun = {
	/** Ends the call for the cause, unless it already settled or ended; the first cause decides its result. */
	readonly interrupt: (cause: EndpointInterruption) => void;
	/** True when invalidating the session ends the call. */
	readonly endsWithGeneration: boolean;
};
