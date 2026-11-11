import type { HistoryPosition } from "../browser/history.ts";
import type { ScrollTarget } from "../browser/scroll.ts";
import type { PageStylesheets } from "../browser/stylesheets.ts";
import type { AnyPatternMatch } from "../generated-types.ts";
import type { DocumentAttrs, HeadSections } from "../protocol/wire.ts";
import type {
	BuildSkew,
	ClientState,
	CommitReason,
	NavResult,
	PageError,
} from "../results.ts";
import type { RendererNeutralViewDef } from "../views.ts";
import type { ClientOutputResources } from "./client-output-resources.ts";

export type Lifecycle =
	| "pre-boot"
	| "booting"
	| "ready"
	| "boot-failed"
	| "disposed";

/** One matched position of a committed page, with the data that its view reads. */
export type RouteEntry = {
	readonly pattern: string;
	readonly params: Readonly<Record<string, string>>;
	readonly splatValues: ReadonlyArray<string>;
	/** The search that the server parsed for the position; undefined when the position has none. */
	readonly input: unknown;
	/** The view's client definition; undefined for a view without one. */
	readonly view: RendererNeutralViewDef | undefined;
	readonly serverOutput: unknown;
	/** Undefined when the view has no client loader, or its loader failed or did not run. */
	readonly clientOutput: unknown;
	readonly clientOutputResources?: ClientOutputResources | undefined;
	/** The public form of this entry, which keeps its identity while the entry's values are unchanged. */
	readonly match: AnyPatternMatch;
};

/** A page's failure and the position of the first position that cannot render. */
export type FrameError = {
	readonly positionIndex: number;
	readonly pattern: string | undefined;
	readonly error: PageError;
};

/** The committed page: what the screen shows and what the public page state describes. */
export type Frame = {
	/** Advances with each frame, so that a late render failure can be matched to the frame that failed. */
	readonly revision: number;
	readonly href: string;
	readonly historyState: unknown;
	/** The history entry's key; undefined when the entry predates the client. */
	readonly entryKey: string | undefined;
	/** The history entry's position; undefined when Vorma did not write the entry. */
	readonly entryPosition: HistoryPosition | undefined;
	/** The history entry's index in the Navigation API's entries; undefined where the browser has no Navigation API. */
	readonly entryNavigationIndex: number | undefined;
	readonly entries: ReadonlyArray<RouteEntry>;
	readonly error: FrameError | undefined;
	readonly head: HeadSections;
	/** The attributes of `html`, `head`, and `body` that Vorma set for this frame. */
	readonly documentAttrs: DocumentAttrs;
	/** The page's managed stylesheets, which a later commit of the same page applies again. */
	readonly stylesheets: PageStylesheets;
	readonly commitReason: CommitReason;
	/**
	 * The time at which the page's preparation completed, after its read, its
	 * modules, and its client loaders. Revalidation on window focus measures the
	 * page's age from it.
	 */
	readonly preparedAt: number;
	/** The sequence of the read that produced the frame. */
	readonly readSequence: number;
	/**
	 * The scroll that the renderer applies once, when the destination exists in
	 * the DOM; `ClientCtx.scrollApplication` records whether it was applied.
	 * For a frame that a navigation committed, the renderer resets focus with
	 * it (`consumeScrollIntent`).
	 */
	readonly scrollIntent: ScrollTarget | undefined;
};

/** The internal state record. The public `ClientState` is derived from it. */
export type CoreSlices = {
	readonly nav: ClientState["nav"];
	readonly lastNavResult: NavResult | undefined;
	readonly revalidation: ClientState["revalidation"];
	readonly prefetches: ClientState["prefetches"];
	readonly apiEndpointCalls: ClientState["apiEndpointCalls"];
	readonly workIndicatorVisible: boolean;
	readonly buildSkew: BuildSkew | undefined;
};

export type CoreState = CoreSlices &
	(
		| {
				readonly lifecycle: "pre-boot" | "booting";
				readonly frame: undefined;
		  }
		| { readonly lifecycle: "ready" | "boot-failed"; readonly frame: Frame }
		| { readonly lifecycle: "disposed"; readonly frame: Frame | undefined }
	);

export const INITIAL_CORE_STATE: CoreState = {
	lifecycle: "pre-boot",
	frame: undefined,
	nav: undefined,
	lastNavResult: undefined,
	revalidation: {
		required: false,
		phase: undefined,
		skipWorkIndicator: false,
		lastResult: undefined,
	},
	prefetches: [],
	apiEndpointCalls: [],
	workIndicatorVisible: false,
	buildSkew: undefined,
};
