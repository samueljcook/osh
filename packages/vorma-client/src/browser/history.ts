import {
	callWithResult,
	createErrorResult,
	type Result,
} from "primitives-ts/results";
import { createRandomId } from "../foundation/random-id.ts";
import { BROWSER_STATE_PROTOCOL } from "../protocol/constants.ts";
import type { ScrollPosition } from "./scroll.ts";

/*
Vorma owns the browser application's history entries. It stores navigation user
state, an entry key, and an entry position in namespaced fields of
history.state, and retains other fields when replacing an entry. Scroll
positions are remembered per entry key in session storage, because a departed
entry cannot be written after the browser moves.

A position is an epoch identifier and an index. The entry that a document boots
on starts a new epoch at index 0, and so does an entry that Vorma writes while
the current entry has no position, such as an entry that a plain fragment link
added. A push from an entry with a position keeps its epoch and adds one to its
index, and a replace keeps both. Within one epoch, the difference between two
indexes is the distance between the two entries. Vorma never gives a position
to an entry that it did not write, so two entries in different epochs have no
known distance from their positions. Where the browser has the Navigation API,
the index of `navigation.currentEntry` gives the distance between any two
entries of the document, including entries that Vorma did not write.
*/

const USER_STATE_FIELD = BROWSER_STATE_PROTOCOL.historyStateFields.userState;
const ENTRY_KEY_FIELD = BROWSER_STATE_PROTOCOL.historyStateFields.entryKey;
const ENTRY_EPOCH_FIELD = BROWSER_STATE_PROTOCOL.historyStateFields.entryEpoch;
const ENTRY_INDEX_FIELD = BROWSER_STATE_PROTOCOL.historyStateFields.entryIndex;
const SCROLL_STORAGE_KEY = BROWSER_STATE_PROTOCOL.scrollStorageKey;
/** Bounds the remembered positions; the browser's own history bound is of this order. */
const REMEMBERED_SCROLL_LIMIT = 50;

/** Where an entry is among the entries that Vorma wrote in one epoch. */
export type HistoryPosition = {
	readonly epoch: string;
	readonly index: number;
};

export type BrowserHistoryEntry = {
	readonly href: string;
	readonly userState: unknown;
	readonly entryKey: string | undefined;
	/** The entry's position; undefined for an entry that Vorma did not write. */
	readonly position: HistoryPosition | undefined;
	/** The index of the entry in the Navigation API's entries; undefined where the browser has no Navigation API. */
	readonly navigationIndex: number | undefined;
};

/**
 * How a document load writes history: `push` adds an entry, `replace`
 * replaces the current entry, and `reload` loads the current entry again.
 */
export type DocumentLoad =
	| { readonly kind: "push"; readonly href: string }
	| { readonly kind: "replace"; readonly href: string }
	| { readonly kind: "reload" };

/**
 * How a write places its entry: `push` adds an entry after the current one,
 * `replace` replaces the current entry and keeps its position, and
 * `replace-in-new-epoch` replaces the current entry and starts a new epoch, as
 * the entry that a document boots on does.
 */
export type HistoryPlacement = "push" | "replace" | "replace-in-new-epoch";

export type HistoryStateError = {
	readonly kind: "history-state-error";
	readonly msg: string;
	readonly cause: unknown;
};

/** The history state as a record of fields; a state that is not an object holds no fields. */
function readStateRecord(value: unknown): Record<string, unknown> {
	return typeof value === "object" && value !== null
		? (value as Record<string, unknown>)
		: {};
}

/** The position that the state's fields hold; undefined when they hold none. */
function readPosition(
	state: Record<string, unknown>,
): HistoryPosition | undefined {
	const epoch = state[ENTRY_EPOCH_FIELD];
	const index = state[ENTRY_INDEX_FIELD];
	return typeof epoch === "string" &&
		epoch !== "" &&
		typeof index === "number" &&
		Number.isSafeInteger(index) &&
		index >= 0
		? { epoch, index }
		: undefined;
}

/** The position of an entry that a write places relative to the current entry's position. */
function selectWrittenPosition(input: {
	readonly current: HistoryPosition | undefined;
	readonly placement: HistoryPlacement;
}): HistoryPosition {
	const { current, placement } = input;
	if (current === undefined || placement === "replace-in-new-epoch") {
		return { epoch: createRandomId(), index: 0 };
	}
	return placement === "push"
		? { epoch: current.epoch, index: current.index + 1 }
		: current;
}

/** The index of the current entry in the Navigation API's entries; undefined where the browser has no Navigation API. */
function readNavigationIndex(): number | undefined {
	const navigation: unknown = Reflect.get(window, "navigation");
	if (typeof navigation !== "object" || navigation === null) {
		return undefined;
	}
	const entry: unknown = Reflect.get(navigation, "currentEntry");
	if (typeof entry !== "object" || entry === null) {
		return undefined;
	}
	const index: unknown = Reflect.get(entry, "index");
	return typeof index === "number" &&
		Number.isSafeInteger(index) &&
		index >= 0
		? index
		: undefined;
}

/** Reads native user state and Vorma's entry fields without imposing a user-data codec. */
export function readBrowserHistoryEntry(): Result<
	BrowserHistoryEntry,
	HistoryStateError
> {
	const read = callWithResult(() => {
		const state = readStateRecord(window.history.state);
		const entryKey = state[ENTRY_KEY_FIELD];
		return {
			href: window.location.href,
			userState: state[USER_STATE_FIELD],
			entryKey: typeof entryKey === "string" ? entryKey : undefined,
			position: readPosition(state),
			navigationIndex: readNavigationIndex(),
		};
	});
	return read.kind === "ok"
		? read
		: createErrorResult({
				kind: "history-state-error",
				msg: "The browser refused the history read",
				cause: read.error,
			});
}

/** Writes a new or replaced history entry and returns the entry that it produced. */
export function writeBrowserHistoryEntry(input: {
	readonly href: string;
	readonly placement: HistoryPlacement;
	readonly userState: unknown;
	readonly entryKey: string;
}): Result<BrowserHistoryEntry, HistoryStateError> {
	const written = callWithResult(() => {
		const current = readStateRecord(window.history.state);
		const position = selectWrittenPosition({
			current: readPosition(current),
			placement: input.placement,
		});
		const next: Record<string, unknown> = {
			[USER_STATE_FIELD]: input.userState,
			[ENTRY_KEY_FIELD]: input.entryKey,
			[ENTRY_EPOCH_FIELD]: position.epoch,
			[ENTRY_INDEX_FIELD]: position.index,
		};
		if (input.placement === "push") {
			window.history.pushState(next, "", input.href);
		} else {
			window.history.replaceState(
				{ ...current, ...next },
				"",
				input.href,
			);
		}
	});
	if (written.kind === "error") {
		return createErrorResult({
			kind: "history-state-error",
			msg: "The browser refused the history write",
			cause: written.error,
		});
	}
	return readBrowserHistoryEntry();
}

type RememberedScroll = ReadonlyArray<readonly [string, ScrollPosition]>;

/** One remembered position when the stored entry has that shape. */
function readRememberedEntry(
	entry: unknown,
): ReadonlyArray<readonly [string, ScrollPosition]> {
	if (!Array.isArray(entry)) {
		return [];
	}
	const key: unknown = entry[0];
	const position: unknown = entry[1];
	if (
		typeof key !== "string" ||
		typeof position !== "object" ||
		position === null ||
		!("x" in position) ||
		!("y" in position) ||
		typeof position.x !== "number" ||
		typeof position.y !== "number" ||
		!Number.isFinite(position.x) ||
		!Number.isFinite(position.y)
	) {
		return [];
	}
	return [[key, { x: position.x, y: position.y }] as const];
}

/** The remembered positions; storage that is absent, unreadable, or not in this shape remembers nothing. */
function readRemembered(): RememberedScroll {
	const stored = callWithResult(() =>
		sessionStorage.getItem(SCROLL_STORAGE_KEY),
	);
	if (stored.kind === "error" || stored.value === null) {
		return [];
	}
	const text = stored.value;
	const parsed = callWithResult(() => JSON.parse(text) as unknown);
	if (parsed.kind === "error" || !Array.isArray(parsed.value)) {
		return [];
	}
	return parsed.value.flatMap(readRememberedEntry);
}

/** Remembers the scroll position of the entry that `key` names. */
export function rememberScroll(key: string, scroll: ScrollPosition): void {
	const entries = readRemembered().filter(([other]) => other !== key);
	entries.push([key, scroll]);
	// A failed storage write must not prevent navigation.
	callWithResult(() => {
		sessionStorage.setItem(
			SCROLL_STORAGE_KEY,
			JSON.stringify(entries.slice(-REMEMBERED_SCROLL_LIMIT)),
		);
	});
}

export function recallScroll(key: string): ScrollPosition | undefined {
	return readRemembered().find(([other]) => other === key)?.[1];
}

/** The URL without its fragment. */
function readUrlWithoutFragment(url: URL): string {
	const fragmentStart = url.href.indexOf("#");
	return fragmentStart === -1 ? url.href : url.href.slice(0, fragmentStart);
}

/**
 * Loads a URL as a document. A push or a replace to a URL that has a fragment
 * and equals the current URL apart from its fragment would only move the
 * browser to the fragment without loading a document, so this function writes
 * that entry with `pushState` or `replaceState` and then reloads it. The
 * result is an error when the browser refused.
 */
export function loadDocument(load: DocumentLoad): Result<void, unknown> {
	return callWithResult(() => {
		if (load.kind === "reload") {
			window.location.reload();
			return;
		}
		const target = new URL(load.href, window.location.href);
		const isFragmentNavigation =
			target.href.includes("#") &&
			readUrlWithoutFragment(target) ===
				readUrlWithoutFragment(new URL(window.location.href));
		if (!isFragmentNavigation) {
			if (load.kind === "push") {
				window.location.assign(target.href);
			} else {
				window.location.replace(target.href);
			}
			return;
		}
		if (load.kind === "push") {
			window.history.pushState(null, "", target.href);
		} else {
			window.history.replaceState(null, "", target.href);
		}
		window.location.reload();
	});
}

/**
 * Moves the browser by a nonzero number of entries; the move completes later,
 * with a `popstate` event. A distance of zero would reload the document.
 */
export function moveHistory(delta: number): Result<void, unknown> {
	return callWithResult(() => window.history.go(delta));
}
