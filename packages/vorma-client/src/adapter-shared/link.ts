import {
	checkNavigableHref,
	computeLinkState,
	createLinkIntent,
	selectLinkStateFacts,
	type LinkStateFacts,
} from "../adapter-api.ts";
import {
	navigate,
	type ClientState,
	type LinkStateRules,
	type NavOptions,
} from "../index.ts";
import { DOCUMENT_PROTOCOL } from "../protocol/constants.ts";

/*
The renderer-neutral part of every adapter's `Link`: the selection of the
client state that a link reads, its state attributes, the decision whether the
browser or the client handles a click, the pointer activation, and the intent
prefetch. Each adapter supplies the anchor, its props, and its event wiring.
*/

export type LinkBehaviorProps = {
	/**
	 * `intent` holds a prefetch of the link's page while a pointer or focus
	 * rests on the link. `none`, or omission, prefetches nothing.
	 */
	readonly prefetch?: "intent" | "none";
	/**
	 * How long a pointer or focus rests on the link before an intent prefetch
	 * starts. Omission waits 100 milliseconds.
	 */
	readonly prefetchDelayMillis?: number;
	/**
	 * True navigates when a mouse or pen button goes down on the link, instead
	 * of at the click that follows. Omission navigates at the click.
	 */
	readonly navigateOnPointerDown?: boolean;
	/**
	 * Search and hash comparisons are opt-in; pathname matching is automatic.
	 * A link receives `aria-current="page"` when its pathname and search equal
	 * the current page's, whether or not the rules compare the search or the
	 * hash. `skipMatching` turns off `aria-current` as well as the active and
	 * pending attributes.
	 */
	readonly linkStateRules?: LinkStateRules;
	readonly replace?: NavOptions["replace"];
	readonly scrollToTop?: NavOptions["scrollToTop"];
	readonly scrollToHash?: NavOptions["scrollToHash"];
	readonly historyState?: unknown;
	readonly skipWorkIndicator?: boolean;
};

/** What a link reads from the client state: the facts of its link state, and whether boot failed. */
export type LinkSelection = {
	readonly linkStateFacts: LinkStateFacts;
	readonly bootFailed: boolean;
};

const linkSelectionByState = new WeakMap<ClientState, LinkSelection>();

/**
 * Selects what a link reads from the client state. Every link receives the
 * same object for the same state, so a state change costs each link one
 * comparison, and a link renders again only when its selection changes.
 */
export function selectLinkSelection(state: ClientState): LinkSelection {
	const known = linkSelectionByState.get(state);
	if (known !== undefined) {
		return known;
	}
	const selection: LinkSelection = {
		linkStateFacts: selectLinkStateFacts(state),
		bootFailed: state.page.error?.kind === "boot-failure",
	};
	linkSelectionByState.set(state, selection);
	return selection;
}

/**
 * The state attributes of a link: the active and pending attributes under the
 * link's rules, and `aria-current`, which is the application's own value when
 * the link has one, `"page"` when the link's pathname and search equal the
 * current page's, and absent otherwise. An absent attribute has the value
 * `undefined`.
 */
export function computeLinkStateAttrs<AriaCurrent>(input: {
	readonly href: string | undefined;
	readonly targetPattern: string | undefined;
	readonly selection: LinkSelection;
	readonly rules: LinkStateRules | undefined;
	readonly ariaCurrent: AriaCurrent | undefined;
}): Readonly<Record<string, AriaCurrent | "" | "page" | undefined>> {
	const names = DOCUMENT_PROTOCOL.attrs;
	const { href, targetPattern, rules } = input;
	const facts = input.selection.linkStateFacts;
	if (href === undefined) {
		return {
			[names.linkActiveExact]: undefined,
			[names.linkActiveAncestor]: undefined,
			[names.linkPendingExact]: undefined,
			[names.linkPendingAncestor]: undefined,
			"aria-current": input.ariaCurrent,
		};
	}
	const state = computeLinkState({ href, targetPattern, facts, rules });
	const isCurrentPage = computeLinkState({
		href,
		targetPattern,
		facts,
		rules: { ...rules, includeSearch: true, includeHash: false },
	}).active.exact;
	return {
		[names.linkActiveExact]: state.active.exact ? "" : undefined,
		[names.linkActiveAncestor]: state.active.ancestor ? "" : undefined,
		[names.linkPendingExact]: state.pending.exact ? "" : undefined,
		[names.linkPendingAncestor]: state.pending.ancestor ? "" : undefined,
		"aria-current":
			input.ariaCurrent ?? (isCurrentPage ? "page" : undefined),
	};
}

/** The fields of a click that link handling reads; React's synthetic events and DOM events both have them. */
export type LinkClickFacts = {
	readonly currentTarget: HTMLAnchorElement;
	readonly defaultPrevented: boolean;
	readonly metaKey: boolean;
	readonly altKey: boolean;
	readonly ctrlKey: boolean;
	readonly shiftKey: boolean;
	readonly button: number;
	/** The click count, which is 0 for a click that a key press or script caused. */
	readonly detail: number;
	readonly preventDefault: () => void;
};

export type LinkPointerDownFacts = LinkClickFacts & {
	readonly pointerType: string;
};

/** Lowercases ASCII letters only, as the HTML standard compares keywords. */
function toAsciiLowercase(text: string): string {
	return text.replace(/[A-Z]/g, (letter) => letter.toLowerCase());
}

/**
 * The anchor's target, as the browser reads it for a navigation: the anchor's
 * own `target` attribute, else the `target` of the document's first `base`
 * element that has one, else the empty string.
 */
function readAnchorTarget(anchor: HTMLAnchorElement): string {
	const own = anchor.getAttribute("target");
	if (own !== null) {
		return own;
	}
	return (
		anchor.ownerDocument
			.querySelector("base[target]")
			?.getAttribute("target") ?? ""
	);
}

/**
 * Whether the browser, and not the client, handles a click on a link's
 * anchor. The browser handles it when a handler prevented its default; when
 * a modifier key is down or a button other than the primary one was pressed;
 * when the anchor's target, which ASCII case does not change, names a browsing
 * context other than the anchor's own (`""` and `_self` name the anchor's
 * own); when the anchor has a `download` attribute; and when the anchor's
 * `href` is missing, does not parse, or is not on the document's origin over
 * HTTP or HTTPS.
 */
export function checkBrowserHandlesLinkClick(event: LinkClickFacts): boolean {
	const anchor = event.currentTarget;
	const target = toAsciiLowercase(readAnchorTarget(anchor));
	const href = anchor.getAttribute("href");
	return (
		event.defaultPrevented ||
		event.metaKey ||
		event.altKey ||
		event.ctrlKey ||
		event.shiftKey ||
		event.button !== 0 ||
		(target !== "" && target !== "_self") ||
		anchor.hasAttribute("download") ||
		href === null ||
		!checkNavigableHref(href)
	);
}

/** The link props that its interaction reads when an event arrives. */
export type LinkBehavior = {
	/** The link's href; undefined for a target that cannot be encoded. */
	readonly href: string | undefined;
	readonly prefetch: LinkBehaviorProps["prefetch"];
	readonly prefetchDelayMillis: number | undefined;
	readonly navigateOnPointerDown: boolean | undefined;
	readonly navOptions: NavOptions;
};

/**
 * The event handling of one link. An adapter calls each handler from the
 * anchor's event, and passes the application's own handler for the event
 * where its order matters.
 */
export type LinkInteraction = {
	/**
	 * Suppresses the click that follows a pointer-down navigation, runs the
	 * application's handler, and then navigates unless the browser handles
	 * the click.
	 */
	readonly handleClick: (input: {
		readonly event: LinkClickFacts;
		readonly callApplicationHandlerFn: () => void;
	}) => void;
	/** Runs the application's handler, and then navigates when the link navigates on pointer-down. */
	readonly handlePointerDown: (input: {
		readonly event: LinkPointerDownFacts;
		readonly callApplicationHandlerFn: () => void;
	}) => void;
	/** Ends a pointer activation, as a key press or a drag does. */
	readonly endPointerActivation: () => void;
	/** Starts an intent prefetch when the link prefetches on intent and its href is navigable. */
	readonly startIntent: (source: "pointer" | "focus") => void;
	readonly endIntent: (source: "pointer" | "focus") => void;
	/** Ends the pointer activation and the pointer's intent, because the browser cancelled the gesture. */
	readonly cancelPointer: () => void;
	/** Releases the intent prefetch, as the link's unmount does. */
	readonly reset: () => void;
};

/** Creates a link's interaction, which reads the link's latest behavior when an event arrives. */
export function createLinkInteraction(input: {
	readonly readBehaviorFn: () => LinkBehavior;
}): LinkInteraction {
	const { readBehaviorFn } = input;
	/** Pointer activation suppresses only the following pointer-generated click. */
	let pointerActivation: "ready" | "awaiting-click" = "ready";
	const intent = createLinkIntent({ getHref: () => readBehaviorFn().href });
	function navigateToLinkHref(): void {
		const behavior = readBehaviorFn();
		if (behavior.href !== undefined) {
			void navigate({ href: behavior.href, ...behavior.navOptions });
		}
	}
	return {
		handleClick: ({ event, callApplicationHandlerFn }) => {
			const suppress =
				pointerActivation === "awaiting-click" && event.detail > 0;
			pointerActivation = "ready";
			if (suppress) {
				event.preventDefault();
			}
			callApplicationHandlerFn();
			if (checkBrowserHandlesLinkClick(event)) {
				return;
			}
			event.preventDefault();
			navigateToLinkHref();
		},
		handlePointerDown: ({ event, callApplicationHandlerFn }) => {
			pointerActivation = "ready";
			callApplicationHandlerFn();
			if (
				readBehaviorFn().navigateOnPointerDown !== true ||
				(event.pointerType !== "mouse" &&
					event.pointerType !== "pen") ||
				checkBrowserHandlesLinkClick(event)
			) {
				return;
			}
			event.preventDefault();
			// Only this gesture's click has already navigated.
			pointerActivation = "awaiting-click";
			navigateToLinkHref();
		},
		endPointerActivation: () => {
			pointerActivation = "ready";
		},
		startIntent: (source) => {
			const behavior = readBehaviorFn();
			if (
				behavior.prefetch === "intent" &&
				checkNavigableHref(behavior.href)
			) {
				intent.start(source, behavior.prefetchDelayMillis);
			}
		},
		endIntent: (source) => intent.end(source),
		cancelPointer: () => {
			pointerActivation = "ready";
			intent.cancelPointer();
		},
		reset: () => intent.reset(),
	};
}
