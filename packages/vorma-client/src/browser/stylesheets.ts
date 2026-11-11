import {
	createResourceScope,
	type ResourceScope,
} from "primitives-ts/resource-scopes";
import {
	createErrorResult,
	createSuccessResult,
	type Result,
} from "primitives-ts/results";
import {
	createSharedResource,
	type SharedResource,
} from "primitives-ts/shared-resources";
import { DOCUMENT_PROTOCOL } from "../protocol/constants.ts";

/*
Managed stylesheets are the stylesheet bundles that Vorma adds to the document
for the pages that client navigation commits. For each bundle that the document
does not have, a preparation inserts a stylesheet link with a media query that
matches no medium, so that the stylesheet loads and the browser parses it
without applying it. In the same task, the preparation inserts a style preload
for the same URL before the stylesheet link. Chromium gives a stylesheet with a
media query that matches no medium a lower request priority than a style
preload, so the preload starts the request at the priority of a style preload,
and the stylesheet link uses the preload's response. The browser makes one
request for the bundle, also when its response is not cacheable. The
preparation waits for the `load` or `error` event of the stylesheet link
itself, which fires after the browser has parsed the stylesheet or has failed
to load it.

At commit, `applyStylesheets` removes each prepared link's media query in the
synchronous commit section, which also changes the DOM for the destination, so
each parsed stylesheet applies in the same task as that DOM change. A
stylesheet that failed to load does not fail the navigation. Its link leaves
the document when the load fails, the commit applies nothing for it, and the
next preparation of that bundle inserts and waits for a new stylesheet.

A preparation returns the page's stylesheets, and the commit applies exactly
those. Two preparations that share a bundle share its stylesheet, so each
commit reads that stylesheet's own state, also after an earlier commit applied
it or after its load failed.

There is no time limit. A stylesheet request that never settles keeps the
preparation, and therefore the navigation, pending until a newer navigation
supersedes it, as a browser's own document navigation waits for its
stylesheets. A new stylesheet link takes the place that the destination's
bundle order gives it among the managed stylesheets that the document has when
the preparation inserts it. Vorma never moves a managed link, because a moved
stylesheet link loads again. The stylesheets of pages that the user left remain
in the document.
*/

const BUNDLE_ATTR = DOCUMENT_PROTOCOL.attrs.cssBundle;
const PRELOAD_ATTR = DOCUMENT_PROTOCOL.attrs.cssPreload;
/** A media query that matches no medium, so that a loaded stylesheet does not apply. */
const UNAPPLIED_MEDIA = "not all";

type PreparationState =
	| {
			readonly kind: "loading";
			/** The stylesheet that the preparations hold. */
			readonly sharedStylesheet: SharedResource;
			/** The style preload that gives the stylesheet's request its priority. */
			readonly priorityPreload: HTMLLinkElement;
			/** Holds the stylesheet link's `load` and `error` listeners. */
			readonly listeners: ResourceScope;
			readonly resolve: () => void;
	  }
	/** The stylesheet loaded, and it does not apply. */
	| { readonly kind: "loaded"; readonly sharedStylesheet: SharedResource }
	/** The stylesheet failed to load, and its link left the document. */
	| { readonly kind: "failed" }
	| { readonly kind: "applied" }
	/** The last hold was released before a commit applied the stylesheet. */
	| { readonly kind: "removed" };

type StylesheetPreparation = {
	readonly path: string;
	readonly link: HTMLLinkElement;
	/** Resolves when the stylesheet link fires `load` or `error`. */
	readonly settled: Promise<void>;
	state: PreparationState;
};

/** One stylesheet of a page: one that the document has applied, or one that a preparation loads. */
type PageStylesheet =
	| { readonly kind: "document"; readonly path: string }
	| {
			readonly kind: "prepared";
			readonly preparation: StylesheetPreparation;
	  };

/** The stylesheets of one page, in the page's bundle order, as a commit applies them. */
export type PageStylesheets = ReadonlyArray<PageStylesheet>;

const preparations = new WeakMap<HTMLLinkElement, StylesheetPreparation>();

/** A commit's bundles that the document cannot apply. */
export type UnpreparedStylesheetsError = {
	readonly kind: "unprepared-stylesheets";
	/** The bundles without an applied stylesheet and without a prepared stylesheet that has loaded or failed to load. */
	readonly bundles: ReadonlyArray<string>;
};

function findLink(input: {
	readonly attr: string;
	readonly value: string;
}): HTMLLinkElement | null {
	for (const link of document.head.querySelectorAll<HTMLLinkElement>(
		"link",
	)) {
		if (link.getAttribute(input.attr) === input.value) {
			return link;
		}
	}
	return null;
}

/** The link that holds a bundle in the document: its applied stylesheet, else its prepared one. */
function findManagedLink(path: string): HTMLLinkElement | null {
	return (
		findLink({ attr: BUNDLE_ATTR, value: path }) ??
		findLink({ attr: PRELOAD_ATTR, value: path })
	);
}

/** Reads the stylesheet bundles that the initial document applied. */
export function readDocumentStylesheets(): PageStylesheets {
	return [...document.head.querySelectorAll<HTMLLinkElement>("link")].flatMap(
		(link): PageStylesheets => {
			const bundle = link.getAttribute(BUNDLE_ATTR);
			return bundle === null ? [] : [{ kind: "document", path: bundle }];
		},
	);
}

/** The bundles of a page's stylesheets, in the page's order. */
export function listStylesheetBundles(
	stylesheets: PageStylesheets,
): ReadonlyArray<string> {
	return stylesheets.map((stylesheet) => {
		return stylesheet.kind === "document"
			? stylesheet.path
			: stylesheet.preparation.path;
	});
}

/**
 * Starts managed stylesheet loads and waits for every load or error without
 * applying destination styles. The scope holds each stylesheet; disposing it
 * releases the holds, and the last release removes a stylesheet that was not
 * applied. `ready` stays pending while a stylesheet request is pending,
 * because the wait has no time limit.
 */
export function prepareStylesheets(
	bundles: ReadonlyArray<string>,
	scope: ResourceScope,
): { readonly stylesheets: PageStylesheets; readonly ready: Promise<void> } {
	const order = [...new Set(bundles)];
	const stylesheets: Array<PageStylesheet> = [];
	const waits: Array<Promise<void>> = [];
	for (const [index, path] of order.entries()) {
		const preparation = acquireStylesheet({ order, index, path, scope });
		if (preparation === undefined) {
			stylesheets.push({ kind: "document", path });
			continue;
		}
		stylesheets.push({ kind: "prepared", preparation });
		waits.push(preparation.settled);
	}
	return {
		stylesheets,
		ready: Promise.all(waits).then(() => undefined),
	};
}

/**
 * The place for a new stylesheet of the bundle at `index` of the destination's
 * order: after the nearest earlier bundle that the document has, else before
 * the nearest later one, else at the end of the head. Prefetch order must not
 * determine the cascade order of newly applied bundles, so the destination's
 * order relative to the managed stylesheets that the document already has,
 * applied or prepared, decides the place.
 */
function findInsertionPoint(input: {
	readonly order: ReadonlyArray<string>;
	readonly index: number;
}): { readonly parent: Node; readonly before: Node | null } {
	for (const earlier of input.order.slice(0, input.index).reverse()) {
		const link = findManagedLink(earlier);
		if (link?.parentNode != null) {
			return { parent: link.parentNode, before: link.nextSibling };
		}
	}
	for (const later of input.order.slice(input.index + 1)) {
		const link = findManagedLink(later);
		if (link?.parentNode != null) {
			return { parent: link.parentNode, before: link };
		}
	}
	return { parent: document.head, before: null };
}

/**
 * The preparation that loads `path`, with a hold that the scope owns, or
 * undefined when the document has applied the bundle. A new preparation's
 * stylesheet starts with its creator's hold, and the scope receives that hold
 * after the links are inserted, so that a scope that is already disposed
 * removes the inserted links at once. A stylesheet that settles while its
 * link is inserted, as a test document can make it do, keeps that hold until
 * the scope releases it.
 */
function acquireStylesheet(input: {
	readonly order: ReadonlyArray<string>;
	readonly index: number;
	readonly path: string;
	readonly scope: ResourceScope;
}): StylesheetPreparation | undefined {
	const { path } = input;
	if (findLink({ attr: BUNDLE_ATTR, value: path }) !== null) {
		return undefined;
	}
	const existing = findLink({ attr: PRELOAD_ATTR, value: path });
	const previous = existing === null ? undefined : preparations.get(existing);
	const previousState = previous?.state;
	if (
		previous !== undefined &&
		(previousState?.kind === "loading" || previousState?.kind === "loaded")
	) {
		const hold = previousState.sharedStylesheet.hold();
		// The type alone requires this check: the shared stylesheet of a loading
		// or loaded preparation is held, because its disposal makes the
		// preparation `removed` in the same call.
		if (hold.kind === "held") {
			input.scope.addCleanup(hold.release);
		}
		return previous;
	}
	existing?.remove();
	const place = findInsertionPoint(input);
	const priorityPreload = document.createElement("link");
	priorityPreload.rel = "preload";
	priorityPreload.as = "style";
	priorityPreload.href = path;
	const link = document.createElement("link");
	link.rel = "stylesheet";
	link.media = UNAPPLIED_MEDIA;
	link.href = path;
	link.setAttribute(PRELOAD_ATTR, path);
	const deferred = Promise.withResolvers<void>();
	const listeners = createResourceScope({
		onCleanupErrorFn: (cause) =>
			console.error("A Vorma stylesheet cleanup failed.", cause),
	});
	const { sharedResource: sharedStylesheet, releaseCreatorHold } =
		createSharedResource({
			disposeFn: () => removeStylesheet(preparation),
		});
	const preparation: StylesheetPreparation = {
		path,
		link,
		settled: deferred.promise,
		state: {
			kind: "loading",
			sharedStylesheet,
			priorityPreload,
			listeners,
			resolve: deferred.resolve,
		},
	};
	function settle(outcome: "loaded" | "failed"): void {
		const state = preparation.state;
		if (state.kind !== "loading") {
			return;
		}
		preparation.state =
			outcome === "loaded"
				? { kind: "loaded", sharedStylesheet: state.sharedStylesheet }
				: { kind: "failed" };
		state.listeners.dispose();
		state.priorityPreload.remove();
		if (outcome === "failed") {
			link.remove();
		}
		state.resolve();
	}
	function handleLoad(): void {
		settle("loaded");
	}
	function handleError(): void {
		settle("failed");
	}
	preparations.set(link, preparation);
	link.addEventListener("load", handleLoad);
	link.addEventListener("error", handleError);
	listeners.addCleanup(() => {
		link.removeEventListener("load", handleLoad);
		link.removeEventListener("error", handleError);
	});
	// The preload starts the request before the stylesheet link asks for the
	// same URL, so the stylesheet link uses the preload's response.
	place.parent.insertBefore(priorityPreload, place.before);
	place.parent.insertBefore(link, place.before);
	input.scope.addCleanup(releaseCreatorHold);
	return preparation;
}

/**
 * Removes a stylesheet that is loading or loaded at the release of its last
 * hold. A stylesheet that failed to load has already left the document, and
 * the document owns an applied stylesheet, so neither changes.
 */
function removeStylesheet(preparation: StylesheetPreparation): void {
	const state = preparation.state;
	if (state.kind !== "loading" && state.kind !== "loaded") {
		return;
	}
	preparation.state = { kind: "removed" };
	if (state.kind === "loading") {
		state.listeners.dispose();
		state.priorityPreload.remove();
	}
	preparation.link.remove();
}

/** Whether the commit can use the page's stylesheet: applied, loaded, or failed to load. */
function checkSettledStylesheet(stylesheet: PageStylesheet): boolean {
	if (stylesheet.kind === "document") {
		return findLink({ attr: BUNDLE_ATTR, value: stylesheet.path }) !== null;
	}
	const { state } = stylesheet.preparation;
	return (
		state.kind === "loaded" ||
		state.kind === "applied" ||
		state.kind === "failed"
	);
}

/**
 * Applies a page's prepared stylesheets in the commit's synchronous section,
 * so that each stylesheet, which the browser has already parsed, applies in
 * the same task as the destination's DOM change. Each stylesheet keeps the
 * place where its preparation inserted it, and a stylesheet that failed to
 * load applies nothing. A stylesheet that is still loading, or one that left
 * the document before the commit, is an error, which names those bundles, and
 * then nothing changes.
 */
export function applyStylesheets(
	stylesheets: PageStylesheets,
): Result<void, UnpreparedStylesheetsError> {
	const unprepared = stylesheets.flatMap((stylesheet) => {
		if (checkSettledStylesheet(stylesheet)) {
			return [];
		}
		return [
			stylesheet.kind === "document"
				? stylesheet.path
				: stylesheet.preparation.path,
		];
	});
	if (unprepared.length > 0) {
		return createErrorResult({
			kind: "unprepared-stylesheets",
			bundles: unprepared,
		});
	}
	for (const stylesheet of stylesheets) {
		if (
			stylesheet.kind !== "prepared" ||
			stylesheet.preparation.state.kind !== "loaded"
		) {
			continue;
		}
		const { preparation } = stylesheet;
		preparation.state = { kind: "applied" };
		preparation.link.removeAttribute(PRELOAD_ATTR);
		preparation.link.setAttribute(BUNDLE_ATTR, preparation.path);
		preparation.link.removeAttribute("media");
	}
	return createSuccessResult(undefined);
}
