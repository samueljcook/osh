import { getErrorMsg } from "primitives-ts/error-msgs";
import { resolveRedirectUrl } from "primitives-ts/http/redirects";
import { callWithResult, mapResultError } from "primitives-ts/results";
import { compareValuesForStructuralEquality } from "primitives-ts/value-comparison";
import {
	loadDocument,
	moveHistory,
	readBrowserHistoryEntry,
	recallScroll,
	rememberScroll,
	writeBrowserHistoryEntry,
	type BrowserHistoryEntry,
	type DocumentLoad,
} from "../browser/history.ts";
import {
	applyScroll,
	readScrollPosition,
	type ScrollTarget,
} from "../browser/scroll.ts";
import {
	listStylesheetBundles,
	prepareStylesheets,
} from "../browser/stylesheets.ts";
import { createRandomId } from "../foundation/random-id.ts";
import type { ClientState, NavResult, NavSource } from "../results.ts";
import type { BeforePageCommitCtx } from "../views.ts";
import { reportBuildSkew } from "./build-skew.ts";
import { getActiveCandidate } from "./candidate-slot.ts";
import { retainClientOutputs } from "./client-output-resources.ts";
import { commitFrame, type HistoryEntryWrite } from "./commit.ts";
import type { Frame } from "./core-state.ts";
import type {
	ArrivedEntryLocation,
	Candidate,
	CandidateRole,
	ClientCtx,
	DepartedEntryLocation,
	NavIntent,
	NavMachine,
} from "./ctx.ts";
import { reloadDocument } from "./document-reload.ts";
import { reportFailure } from "./events.ts";
import {
	abandonAttempt,
	failAttempt,
	getFreshnessAttempt,
	retireDemand,
	satisfyDemand,
	transferAttempt,
} from "./freshness.ts";
import { previewPage, runPageCommit } from "./page-commit.ts";
import {
	retirePreparationsAndOutputs,
	takePrefetchForNav,
} from "./prefetch.ts";
import {
	rerunClientLoaders,
	startPreparation,
	type Preparation,
	type PreparationOutcome,
	type PreparedPage,
} from "./prepare.ts";
import { findRetainedOutputs, retainOutputs } from "./retained-outputs.ts";
import {
	applyFreshness,
	deriveRevalidationSlice,
	maybeStartAttempt,
} from "./revalidation.ts";
import { patchState, updateState } from "./state.ts";
import {
	checkJavascriptUrl,
	checkNavigableOrigin,
	readDestination,
} from "./targets.ts";

/*
Navigation and revalidation share one machine: at most one candidate occupies
the slot, a new candidate replaces the occupant before the occupant is closed,
and only the candidate that occupies the slot can commit. A commit checks the
slot, the lifecycle, and the generation in one synchronous section, so nothing
can interleave between the check and the commit. A candidate's role says what it
is; a redirect gives a navigation a new role.
*/

const REDIRECT_LIMIT = 20;

/** A navigation that `navigate`, URL synchronization, or a redirect starts. */
export type NavInput = NavIntent & {
	readonly replace: boolean;
	readonly source: "navigate" | "redirect";
	/**
	 * `shared` lets the navigation join a pending navigation with the same
	 * intent or take the read of a pending navigation to the same destination.
	 * `own` makes it read its destination itself, as a mutation's redirect that
	 * owes a revalidation must because a pending read can predate the
	 * mutation.
	 */
	readonly read: "shared" | "own";
};

type NavRole = Exclude<CandidateRole, { readonly kind: "revalidation" }>;

type CandidateEnd =
	| { readonly kind: "committed"; readonly frame: Frame }
	| { readonly kind: "superseded" }
	| { readonly kind: "cancelled" }
	| {
			readonly kind: "document-nav";
			readonly href: string;
			readonly buildSkew: boolean;
	  }
	/** A revalidation read redirected, so a navigation took over. */
	| { readonly kind: "redirected" }
	| {
			readonly kind: "failure";
			readonly stage: "request" | "modules" | "commit" | "document";
			readonly cause: unknown;
	  };

// Roles

/** The public navigation source of a role; a revalidation has none. */
function selectNavSource(role: CandidateRole): NavSource | undefined {
	switch (role.kind) {
		case "push":
		case "replace":
			return role.source;
		case "traversal":
			return "popstate";
		case "redirected-traversal":
			return "redirect";
		case "revalidation":
			return undefined;
	}
}

function selectCommitReason(
	role: CandidateRole,
): BeforePageCommitCtx["reason"] {
	switch (role.kind) {
		case "push":
		case "replace":
			return "nav";
		case "traversal":
		case "redirected-traversal":
			return "popstate";
		case "revalidation":
			return "revalidation";
	}
}

/** How a commit for the role writes history. */
function selectHistoryEntryWrite(role: CandidateRole): HistoryEntryWrite {
	switch (role.kind) {
		case "push":
			return { kind: "write", placement: "push" };
		case "replace":
		case "redirected-traversal":
			return { kind: "write", placement: "replace" };
		case "traversal":
		case "revalidation":
			return { kind: "adopt" };
	}
}

/**
 * The role that a redirect gives a navigation. A redirect chain writes its
 * final URL once and keeps the requested history mode, also when the chain
 * ends at the committed destination, as a browser's document navigation adds
 * an entry for a URL that redirects to the current URL. A traversal's redirect
 * replaces the entry that the browser arrived at.
 */
function selectRedirectedRole(role: NavRole): NavRole {
	switch (role.kind) {
		case "push":
			return { kind: "push", source: "redirect" };
		case "replace":
			return { kind: "replace", source: "redirect" };
		case "traversal":
		case "redirected-traversal":
			return {
				kind: "redirected-traversal",
				departed: role.departed,
				arrived: role.arrived,
			};
	}
}

// Starting candidates

function createCandidate(
	ctx: ClientCtx,
	input: {
		readonly role: CandidateRole;
		readonly intent: NavIntent;
		readonly observers: Array<(result: NavResult) => void>;
		readonly preparation: Preparation | undefined;
	},
): Candidate {
	const candidate: Candidate = {
		scope: ctx.scope.createChild(),
		role: input.role,
		intent: input.intent,
		generation: ctx.generation,
		preparation:
			input.preparation === undefined
				? { kind: "vacant" }
				: { kind: "owned", preparation: input.preparation },
		completion: { kind: "pending", observers: input.observers },
	};
	candidate.scope.addCleanup(() => {
		// Disposes any preparation that this candidate still owns.
		takePreparation(candidate)?.scope.dispose();
	});
	return candidate;
}

/** Projects the public `nav` state slice from the navigation machine, keeping identity when unchanged. */
function deriveNavSlice(input: {
	readonly machine: NavMachine;
	readonly current: ClientState["nav"];
}): ClientState["nav"] {
	const { machine, current } = input;
	if (machine.kind !== "running") {
		return undefined;
	}
	const source = selectNavSource(machine.candidate.role);
	if (source === undefined) {
		return undefined;
	}
	const href = machine.candidate.intent.href;
	const skipWorkIndicator = machine.candidate.intent.skipWorkIndicator;
	if (
		current !== undefined &&
		current.href === href &&
		current.source === source &&
		current.skipWorkIndicator === skipWorkIndicator
	) {
		return current;
	}
	return { href, source, skipWorkIndicator };
}

/**
 * Puts a candidate in the slot, replaces the occupant, and starts it. A
 * running attempt passes to the candidate that takes the slot.
 */
function activateCandidate(ctx: ClientCtx, candidate: Candidate): void {
	const previous = getActiveCandidate(ctx);
	ctx.navMachine = { kind: "running", candidate };
	ctx.freshness = transferAttempt(
		ctx.freshness,
		candidate.role.kind === "revalidation" ? "revalidation" : "navigation",
	);
	updateState(ctx, (state) => {
		const nextNav = deriveNavSlice({
			machine: ctx.navMachine,
			current: state.nav,
		});
		return nextNav === state.nav ? state : { ...state, nav: nextNav };
	});
	if (previous !== undefined) {
		endSuperseded(previous);
	}
	void startCandidate(ctx, candidate);
}

/** Closes a candidate that a successor replaced. Its callers receive `superseded`. */
function endSuperseded(candidate: Candidate): void {
	endCandidate(candidate, { kind: "superseded" });
}

function endCandidate(candidate: Candidate, result: NavResult): void {
	const completion = candidate.completion;
	if (completion.kind === "ended") {
		return;
	}
	const accepted =
		completion.kind === "accepted" ? completion.result : result;
	candidate.completion = { kind: "ended", result: accepted };
	candidate.scope.dispose();
	settleObservers(completion.observers, accepted);
}

function acceptCandidate(candidate: Candidate, result: NavResult): void {
	const completion = candidate.completion;
	if (completion.kind !== "pending") {
		return;
	}
	candidate.completion = {
		kind: "accepted",
		result,
		observers: completion.observers,
	};
}

function settleObservers(
	observers: Array<(result: NavResult) => void>,
	result: NavResult,
): void {
	const pending = observers.splice(0);
	for (const observer of pending) {
		observer(result);
	}
}

/**
 * Closes the occupant of the slot when nothing replaces it, as a commit that
 * needs no read does. A running attempt returns to its demand, and the caller
 * starts the next attempt once the browser shows the page that it must read.
 */
function vacateSlot(ctx: ClientCtx): void {
	const occupant = getActiveCandidate(ctx);
	if (occupant === undefined) {
		if (ctx.navMachine.kind === "returning-history") {
			ctx.navMachine = { kind: "idle" };
		}
		return;
	}
	ctx.navMachine = { kind: "idle" };
	ctx.freshness = abandonAttempt(ctx.freshness).facts;
	updateState(ctx, (state) => {
		const nextNav = deriveNavSlice({
			machine: ctx.navMachine,
			current: state.nav,
		});
		const nextRevalidation = deriveRevalidationSlice({
			facts: ctx.freshness,
			current: state.revalidation,
		});
		return nextNav === state.nav && nextRevalidation === state.revalidation
			? state
			: { ...state, nav: nextNav, revalidation: nextRevalidation };
	});
	endSuperseded(occupant);
}

function notifyNavStart(ctx: ClientCtx, source: NavSource): void {
	for (const listener of Array.from(ctx.navStartListeners)) {
		listener(source);
	}
}

// Targets and scroll

function checkSameDestination(input: {
	readonly left: URL;
	readonly right: URL;
}): boolean {
	return readDestination(input.left) === readDestination(input.right);
}

/**
 * How a candidate with the role loads `href` as a document. A traversal
 * reloads because the browser is already on its entry. A replace, a
 * redirected traversal, and a revalidation's redirect replace the current
 * entry, and a push adds an entry.
 */
function selectDocumentLoad(input: {
	readonly role: CandidateRole;
	readonly href: string;
}): DocumentLoad {
	switch (input.role.kind) {
		case "push":
			return { kind: "push", href: input.href };
		case "replace":
		case "redirected-traversal":
		case "revalidation":
			return { kind: "replace", href: input.href };
		case "traversal":
			return { kind: "reload" };
	}
}

function decideScrollIntent(input: {
	readonly intent: NavIntent;
	readonly url: URL;
	readonly restoreScroll: ScrollTarget | undefined;
	readonly isNewDestination: boolean;
}): ScrollTarget | undefined {
	const { intent, url } = input;
	if (input.restoreScroll !== undefined) {
		return input.restoreScroll;
	}
	if (url.hash !== "" && intent.scrollToHash !== false) {
		return {
			hash: url.hash,
			scrollToTopIfMissing:
				input.isNewDestination && intent.scrollToTop !== false,
		};
	}
	return intent.scrollToTop === false ? undefined : { x: 0, y: 0 };
}

// Navigating

/**
 * Starts or joins a navigation and resolves with its result. A navigation
 * that `navigate` or URL synchronization starts to the committed destination
 * changes only the hash or the history state, so it commits without a read. A
 * redirect to the committed destination reads it because the redirect can
 * follow a change of the data that the page shows.
 */
export function startNav(ctx: ClientCtx, input: NavInput): Promise<NavResult> {
	const lifecycle = ctx.state.getValue().lifecycle;
	const frame = ctx.state.getValue().frame;
	if (
		lifecycle !== "ready" ||
		frame === undefined ||
		ctx.navMachine.kind === "disposed"
	) {
		return Promise.resolve({ kind: "client-disposed" });
	}
	const parsed = callWithResult(
		() => new URL(input.href, window.location.href),
	);
	if (parsed.kind === "error") {
		return Promise.resolve({
			kind: "invalid-target",
			msg: `The href ${JSON.stringify(input.href)} is not a URL.`,
		});
	}
	const url = parsed.value;
	if (checkJavascriptUrl(url)) {
		return Promise.resolve({
			kind: "invalid-target",
			msg: "navigate does not accept a javascript: URL.",
		});
	}
	if (!checkNavigableOrigin(url)) {
		// A navigation of any destination cancels a pending URL synchronization
		// target, even one that loads another document: the page can stay.
		notifyNavStart(ctx, input.source);
		// The latest navigation intent wins: a pending navigation must not
		// commit and write history while the document leaves.
		if (getActiveCandidate(ctx)?.role.kind !== "revalidation") {
			vacateSlot(ctx);
		}
		const left = loadDocument(
			input.replace
				? { kind: "replace", href: url.href }
				: { kind: "push", href: url.href },
		);
		// The document can stay, for example for a download, so an outstanding
		// demand continues.
		maybeStartAttempt(ctx);
		return Promise.resolve(
			left.kind === "ok"
				? { kind: "document-nav", href: url.href }
				: { kind: "failure", stage: "request", cause: left.error },
		);
	}
	const cloned = callWithResult(() => structuredClone(input.historyState));
	if (cloned.kind === "error") {
		return Promise.resolve({
			kind: "invalid-history-state",
			msg:
				getErrorMsg(cloned.error) ??
				"The history state cannot be cloned.",
		});
	}
	notifyNavStart(ctx, input.source);
	const intent: NavIntent = {
		href: url.href,
		historyState: cloned.value,
		scrollToTop: input.scrollToTop,
		scrollToHash: input.scrollToHash,
		skipWorkIndicator: input.skipWorkIndicator,
	};
	const role: NavRole = input.replace
		? { kind: "replace", source: input.source }
		: { kind: "push", source: input.source };
	const active = getActiveCandidate(ctx);
	if (
		input.source === "navigate" &&
		checkSameDestination({ left: url, right: new URL(frame.href) })
	) {
		return Promise.resolve(
			commitToCommittedDestination(ctx, {
				url,
				intent,
				role,
				leaveRunning: active?.role.kind === "revalidation",
			}),
		);
	}
	if (
		input.read === "shared" &&
		active !== undefined &&
		active.role.kind === role.kind &&
		checkSameIntent({ left: active.intent, right: intent })
	) {
		return new Promise((resolve) => {
			const completion = active.completion;
			if (completion.kind === "ended") {
				resolve(completion.result);
			} else {
				completion.observers.push(resolve);
			}
		});
	}
	return new Promise((resolve) => {
		// A changed intent owns its hooks and commit. It can take the page read
		// for the same destination without keeping the previous intent alive.
		const taken =
			input.read === "shared" &&
			active !== undefined &&
			active.role.kind !== "revalidation" &&
			checkSameDestination({
				left: url,
				right: new URL(active.intent.href),
			})
				? takePreparation(active)
				: undefined;
		activateCandidate(
			ctx,
			createCandidate(ctx, {
				role,
				intent,
				observers: [resolve],
				preparation: taken,
			}),
		);
	});
}

function takePreparation(candidate: Candidate): Preparation | undefined {
	const ownership = candidate.preparation;
	candidate.preparation = { kind: "vacant" };
	return ownership.kind === "owned" ? ownership.preparation : undefined;
}

function checkSameIntent(input: {
	readonly left: NavIntent;
	readonly right: NavIntent;
}): boolean {
	const { left, right } = input;
	return (
		left.href === right.href &&
		left.scrollToTop === right.scrollToTop &&
		left.scrollToHash === right.scrollToHash &&
		left.skipWorkIndicator === right.skipWorkIndicator &&
		compareValuesForStructuralEquality({
			left: left.historyState,
			right: right.historyState,
		})
	);
}

/**
 * A navigation to the committed destination that changes only the hash or
 * the history state needs no read. It is unchanged when nothing differs, where
 * history states are compared by their contents; otherwise it commits locally,
 * without hooks, for the new hash or history state. The latest navigation
 * intent wins: a pending navigation in the slot ends as superseded so that
 * the page stays where this navigation puts it. A running revalidation stays
 * in the slot when a navigation that the user started commits because the
 * page that it reads is the committed destination. A history traversal always
 * replaces the occupant. An attempt that the replaced occupant carried starts
 * again after the commit so that it reads the entry that the commit wrote.
 */
function commitToCommittedDestination(
	ctx: ClientCtx,
	input: {
		readonly url: URL;
		readonly intent: NavIntent;
		readonly role: Extract<
			CandidateRole,
			{ readonly kind: "push" | "replace" | "traversal" }
		>;
		readonly leaveRunning: boolean;
	},
): NavResult {
	const frame = ctx.state.getValue().frame;
	if (frame === undefined) {
		return { kind: "client-disposed" };
	}
	const { url, intent, role } = input;
	const position = readBrowserHistoryEntry();
	const sameEntry =
		position.kind === "ok" &&
		position.value.entryKey !== undefined &&
		position.value.entryKey === frame.entryKey;
	const isUnchanged =
		role.kind === "push" &&
		sameEntry &&
		new URL(frame.href).hash === url.hash &&
		compareValuesForStructuralEquality({
			left: frame.historyState,
			right: intent.historyState,
		});
	if (!input.leaveRunning) {
		vacateSlot(ctx);
	}
	if (isUnchanged) {
		const scrollIntent = decideScrollIntent({
			intent,
			url,
			restoreScroll: undefined,
			isNewDestination: false,
		});
		if (scrollIntent !== undefined) {
			applyScroll(scrollIntent);
		}
		recordNavResult(ctx, { kind: "unchanged", href: frame.href });
		maybeStartAttempt(ctx);
		return { kind: "unchanged", href: frame.href };
	}
	if (role.kind !== "traversal" && frame.entryKey !== undefined) {
		rememberScroll(frame.entryKey, readScrollPosition());
	}
	const committed = commitFrame(ctx, {
		prepared: toPreparedPage(ctx, frame),
		href: intent.href,
		historyState: intent.historyState,
		reason: selectCommitReason(role),
		documentUpdate: "skip",
		historyEntry: selectHistoryEntryWrite(role),
		scrollIntent: decideScrollIntent({
			intent,
			url,
			restoreScroll:
				role.kind === "traversal" ? role.restoreScroll : undefined,
			isNewDestination: false,
		}),
		stateFields: {
			...(input.leaveRunning ? {} : { nav: undefined }),
			lastNavResult: { kind: "committed", href: intent.href },
		},
	});
	if (committed.kind === "error") {
		reloadDocument(ctx, committed.error.cause);
		return {
			kind: "failure",
			stage: committed.error.stage,
			cause: committed.error.cause,
		};
	}
	maybeStartAttempt(ctx);
	return { kind: "committed", href: committed.value.href };
}

function toPreparedPage(ctx: ClientCtx, frame: Frame): PreparedPage {
	return {
		kind: "prepared",
		destination: readDestination(new URL(frame.href)),
		entries: frame.entries,
		error: frame.error,
		head: frame.head,
		documentAttrs: frame.documentAttrs,
		stylesheets: frame.stylesheets,
		readSequence: frame.readSequence,
		generation: ctx.generation,
		preparedAt: frame.preparedAt,
		historyState: frame.historyState,
		records: [],
		serverFailure: undefined,
	};
}

function recordNavResult(ctx: ClientCtx, result: NavResult): void {
	patchState(ctx, { lastNavResult: result });
}

// Popstate

/** Checks whether a restoration `popstate` arrived back at the departed history entry. */
function checkReturnedToDepartedEntry(input: {
	readonly departed: DepartedEntryLocation;
	readonly arrived: BrowserHistoryEntry;
}): boolean {
	const { departed, arrived } = input;
	if (
		departed.navigationIndex !== undefined &&
		arrived.navigationIndex !== undefined
	) {
		return arrived.navigationIndex === departed.navigationIndex;
	}
	if (arrived.position !== undefined && departed.position !== undefined) {
		return (
			arrived.position.epoch === departed.position.epoch &&
			arrived.position.index === departed.position.index &&
			(departed.entryKey === undefined ||
				arrived.entryKey === departed.entryKey)
		);
	}
	if (arrived.entryKey !== undefined) {
		return arrived.entryKey === departed.entryKey;
	}
	return arrived.position === undefined;
}

/** Handles the browser's Back and Forward. */
export function handlePopstate(ctx: ClientCtx): void {
	const frame = ctx.state.getValue().frame;
	if (
		ctx.state.getValue().lifecycle !== "ready" ||
		frame === undefined ||
		ctx.navMachine.kind === "disposed"
	) {
		return;
	}
	const position = readBrowserHistoryEntry();
	if (position.kind === "error") {
		reloadDocument(ctx, position.error);
		return;
	}
	const arrived = position.value;
	if (ctx.navMachine.kind === "returning-history") {
		const returning = ctx.navMachine;
		ctx.navMachine = { kind: "idle" };
		if (
			checkReturnedToDepartedEntry({
				departed: returning.departed,
				arrived,
			})
		) {
			if (returning.phase === "awaiting-popstate") {
				maybeStartAttempt(ctx);
			}
			return;
		}
	}
	if (
		arrived.entryKey !== undefined &&
		arrived.entryKey === frame.entryKey &&
		arrived.href === frame.href
	) {
		// The browser returned to the committed entry. A traversal in flight
		// has nothing left to show, and an attempt that waited for the browser
		// to return can read the committed page.
		const occupant = getActiveCandidate(ctx);
		if (
			occupant?.role.kind === "traversal" ||
			occupant?.role.kind === "redirected-traversal"
		) {
			vacateSlot(ctx);
		}
		maybeStartAttempt(ctx);
		return;
	}
	notifyNavStart(ctx, "popstate");
	// A history traversal replaces whatever was in flight. Entries that other
	// code wrote with the committed entry's state share its key, and the scroll
	// position of an entry that the traversal keeps is not the departed one.
	if (frame.entryKey !== undefined && frame.entryKey !== arrived.entryKey) {
		rememberScroll(frame.entryKey, readScrollPosition());
	}
	const url = new URL(arrived.href);
	const intent: NavIntent = {
		href: arrived.href,
		historyState: arrived.userState,
		scrollToTop: undefined,
		scrollToHash: undefined,
		skipWorkIndicator: false,
	};
	const role = {
		kind: "traversal",
		departed: {
			position: frame.entryPosition,
			navigationIndex: frame.entryNavigationIndex,
		},
		arrived: {
			href: arrived.href,
			entryKey: arrived.entryKey,
			position: arrived.position,
			navigationIndex: arrived.navigationIndex,
		},
		restoreScroll:
			arrived.entryKey === undefined
				? undefined
				: recallScroll(arrived.entryKey),
	} as const;
	if (checkSameDestination({ left: url, right: new URL(frame.href) })) {
		commitToCommittedDestination(ctx, {
			url,
			intent,
			role,
			leaveRunning: false,
		});
		return;
	}
	activateCandidate(
		ctx,
		createCandidate(ctx, {
			role,
			intent,
			observers: [],
			preparation: undefined,
		}),
	);
}

// Revalidation candidates

/**
 * Starts the read that satisfies an outstanding demand. It reads the browser's
 * current URL and history state; when the browser refuses that read, the
 * attempt fails.
 */
export function startRevalidationCandidate(
	ctx: ClientCtx,
	input: { readonly skipWorkIndicator: boolean },
): void {
	if (ctx.navMachine.kind === "disposed") {
		return;
	}
	const position = readBrowserHistoryEntry();
	if (position.kind === "error") {
		applyFreshness(
			ctx,
			failAttempt(ctx.freshness, {
				kind: "failure",
				reason: "page",
				cause: position.error,
			}),
		);
		return;
	}
	activateCandidate(
		ctx,
		createCandidate(ctx, {
			role: { kind: "revalidation" },
			intent: {
				href: position.value.href,
				historyState: position.value.userState,
				scrollToTop: undefined,
				scrollToHash: undefined,
				skipWorkIndicator: input.skipWorkIndicator,
			},
			observers: [],
			preparation: undefined,
		}),
	);
}

/**
 * Restarts the pending navigation with its role, intent, and callers so that
 * a new read replaces its current read.
 */
export function restartPendingNav(ctx: ClientCtx): void {
	const pending = getActiveCandidate(ctx);
	if (pending === undefined || pending.role.kind === "revalidation") {
		return;
	}
	const restarted = createCandidate(ctx, {
		role: pending.role,
		intent: pending.intent,
		observers:
			pending.completion.kind === "ended"
				? []
				: pending.completion.observers.splice(0),
		preparation: undefined,
	});
	activateCandidate(ctx, restarted);
}

/**
 * Restarts the candidate in the slot with a new read so that its commit uses
 * the modules that a development update replaced. A navigation keeps its
 * callers, and a revalidation's attempt starts again with a new revalidation
 * read.
 */
export function restartActiveCandidate(ctx: ClientCtx): void {
	const occupant = getActiveCandidate(ctx);
	if (occupant === undefined) {
		return;
	}
	if (occupant.role.kind === "revalidation") {
		vacateSlot(ctx);
		maybeStartAttempt(ctx);
		return;
	}
	restartPendingNav(ctx);
}

/** Closes the active candidate when the client is disposed; its callers receive `client-disposed`. */
export function closeActiveCandidate(ctx: ClientCtx): void {
	const occupant = getActiveCandidate(ctx);
	ctx.navMachine = { kind: "disposed" };
	if (occupant === undefined) {
		return;
	}
	endCandidate(occupant, { kind: "client-disposed" });
}

// Running a candidate

async function startCandidate(
	ctx: ClientCtx,
	candidate: Candidate,
): Promise<void> {
	if (
		candidate.completion.kind === "ended" ||
		candidate.scope.signal.aborted
	) {
		return;
	}
	const end = await runCandidate(ctx, candidate);
	finishCandidate(ctx, candidate, end);
}

/** The preparation for a candidate's current URL: retained outputs, a taken prefetch, or a new read. */
async function obtainPreparationOutcome(
	ctx: ClientCtx,
	candidate: Candidate,
	url: URL,
): Promise<PreparationOutcome> {
	if (candidate.scope.signal.aborted) {
		return { kind: "aborted" };
	}
	const destination = readDestination(url);
	if (
		candidate.preparation.kind === "owned" &&
		candidate.preparation.preparation.destination === destination
	) {
		return candidate.preparation.preparation.outcome;
	}
	takePreparation(candidate)?.scope.dispose();
	if (candidate.role.kind !== "revalidation") {
		const retained = findRetainedOutputs(ctx, destination);
		const taken = takePrefetchForNav(ctx, destination);
		if (
			retained !== undefined &&
			compareValuesForStructuralEquality({
				left: retained.historyState,
				right: candidate.intent.historyState,
			}) &&
			(taken === undefined ||
				retained.readSequence > taken.preparation.startSequence)
		) {
			candidate.scope.addCleanup(retainClientOutputs(retained.entries));
			taken?.preparation.scope.dispose();
			// The retained page's stylesheets are prepared again so that a
			// stylesheet that failed to load is requested again and waited for,
			// as a new preparation does; an applied stylesheet is ready at once.
			const stylesheets = prepareStylesheets(
				listStylesheetBundles(retained.stylesheets),
				candidate.scope,
			);
			await stylesheets.ready;
			if (candidate.scope.signal.aborted) {
				return { kind: "aborted" };
			}
			return { ...retained, stylesheets: stylesheets.stylesheets };
		}
		if (taken !== undefined) {
			candidate.preparation = {
				kind: "owned",
				preparation: taken.preparation,
			};
			if (candidate.scope.signal.aborted) {
				takePreparation(candidate)?.scope.dispose();
			}
			return taken.preparation.outcome;
		}
	}
	const preparation = startPreparation(ctx, {
		url,
		requestKind:
			candidate.role.kind === "revalidation" ? "revalidation" : "nav",
		historyState: candidate.intent.historyState,
		mayPrestartLoaders: true,
	});
	candidate.preparation = { kind: "owned", preparation };
	if (candidate.scope.signal.aborted) {
		takePreparation(candidate)?.scope.dispose();
	}
	return preparation.outcome;
}

/** The end of a candidate that loads `href` as a document with the history write that its role needs. */
function leaveDocumentForCandidate(input: {
	readonly role: CandidateRole;
	readonly href: string;
	readonly buildSkew: boolean;
}): CandidateEnd {
	const left = loadDocument(
		selectDocumentLoad({ role: input.role, href: input.href }),
	);
	return left.kind === "ok"
		? { kind: "document-nav", href: input.href, buildSkew: input.buildSkew }
		: { kind: "failure", stage: "request", cause: left.error };
}

async function runCandidate(
	ctx: ClientCtx,
	candidate: Candidate,
): Promise<CandidateEnd> {
	let redirects = 0;
	for (;;) {
		const url = new URL(candidate.intent.href);
		let outcome = await obtainPreparationOutcome(ctx, candidate, url);
		if (candidate.scope.signal.aborted) {
			return { kind: "superseded" };
		}
		if (
			outcome.kind === "prepared" &&
			!compareValuesForStructuralEquality({
				left: outcome.historyState,
				right: candidate.intent.historyState,
			}) &&
			outcome.entries.some(
				(entry) => entry.view?.clientLoader !== undefined,
			)
		) {
			// Output that was computed for another history state is computed again
			// over the same server answer.
			outcome = await rerunClientLoaders(ctx, {
				prepared: outcome,
				url,
				historyState: candidate.intent.historyState,
				scope: candidate.scope.createChild(),
			});
			if (candidate.scope.signal.aborted) {
				return { kind: "superseded" };
			}
		}
		switch (outcome.kind) {
			case "prepared":
				return acceptPrepared(ctx, candidate, outcome);
			case "aborted":
				return { kind: "superseded" };
			case "failure":
				return {
					kind: "failure",
					stage: outcome.stage,
					cause: outcome.cause,
				};
			case "document":
				if (candidate.role.kind === "revalidation") {
					return {
						kind: "failure",
						stage: "request",
						cause: new Error(
							"The page request answered with a response that is not Vorma's.",
						),
					};
				}
				return leaveDocumentForCandidate({
					role: candidate.role,
					href: url.href,
					buildSkew: false,
				});
			case "build-skew":
				reportBuildSkew(ctx, outcome.skew);
				retirePreparationsAndOutputs(ctx);
				// A navigation loads the destination as a document from the
				// server's current build; a revalidation fails with
				// "build-skew" and leaves the page in place.
				if (candidate.role.kind === "revalidation") {
					return {
						kind: "document-nav",
						href: url.href,
						buildSkew: true,
					};
				}
				return leaveDocumentForCandidate({
					role: candidate.role,
					href: url.href,
					buildSkew: true,
				});
			case "redirect": {
				const target = mapResultError(
					resolveRedirectUrl({
						location: outcome.href,
						requestUrl: url,
					}),
					(error) => error.cause,
				);
				if (target.kind === "error") {
					return {
						kind: "failure",
						stage: "request",
						cause: target.error,
					};
				}
				if (
					target.value.protocol !== "http:" &&
					target.value.protocol !== "https:"
				) {
					return {
						kind: "failure",
						stage: "request",
						cause: new Error(
							`A redirect named a non-HTTP target: ${target.value.href}`,
						),
					};
				}
				const role = candidate.role;
				if (!checkNavigableOrigin(target.value)) {
					// The document load writes history as the redirected
					// navigation would.
					return leaveDocumentForCandidate({
						role:
							role.kind === "revalidation"
								? role
								: selectRedirectedRole(role),
						href: target.value.href,
						buildSkew: false,
					});
				}
				if (role.kind === "revalidation") {
					// The redirect starts a navigation, which replaces this attempt.
					// The demand ends first because the navigation reads its own
					// destination and its commit must not satisfy the demand.
					applyFreshness(
						ctx,
						retireDemand(ctx.freshness, { kind: "cancelled" }),
					);
					void startNav(ctx, {
						href: target.value.href,
						replace: true,
						historyState: undefined,
						scrollToTop: undefined,
						scrollToHash: undefined,
						skipWorkIndicator: candidate.intent.skipWorkIndicator,
						source: "redirect",
						read: "shared",
					});
					return { kind: "redirected" };
				}
				redirects += 1;
				if (redirects > REDIRECT_LIMIT) {
					return {
						kind: "failure",
						stage: "request",
						cause: new Error(
							`More than ${REDIRECT_LIMIT} redirects.`,
						),
					};
				}
				// A redirect to any destination, including the committed one, is
				// read like any other destination.
				takePreparation(candidate)?.scope.dispose();
				candidate.role = selectRedirectedRole(role);
				candidate.intent = {
					...candidate.intent,
					href: target.value.href,
				};
				updateState(ctx, (state) => {
					const nextNav = deriveNavSlice({
						machine: ctx.navMachine,
						current: state.nav,
					});
					return nextNav === state.nav
						? state
						: { ...state, nav: nextNav };
				});
			}
		}
	}
}

async function acceptPrepared(
	ctx: ClientCtx,
	candidate: Candidate,
	prepared: PreparedPage,
): Promise<CandidateEnd> {
	const reason = selectCommitReason(candidate.role);
	const url = new URL(candidate.intent.href);
	const decision = await runPageCommit(ctx, {
		candidate,
		prepared,
		reason,
		next: previewPage({
			prepared,
			href: candidate.intent.href,
			historyState: candidate.intent.historyState,
			reason,
		}),
		commitNowFn: () => {
			if (
				candidate.scope.signal.aborted ||
				getActiveCandidate(ctx) !== candidate ||
				ctx.state.getValue().lifecycle !== "ready" ||
				prepared.generation !== ctx.generation
			) {
				return undefined;
			}
			const frame = ctx.state.getValue().frame;
			const role = candidate.role;
			const historyEntry = selectHistoryEntryWrite(role);
			if (historyEntry.kind === "adopt") {
				const position = readBrowserHistoryEntry();
				if (
					position.kind === "ok" &&
					readDestination(new URL(position.value.href)) !==
						readDestination(url)
				) {
					// The browser left the destination while the page was prepared.
					return undefined;
				}
			} else if (frame?.entryKey !== undefined) {
				rememberScroll(frame.entryKey, readScrollPosition());
			}
			const isNewDestination =
				frame === undefined ||
				readDestination(new URL(frame.href)) !== readDestination(url);
			const committed = commitFrame(ctx, {
				prepared,
				beforePublishFn: () => {
					// A subscriber can start a successor while the frame publishes,
					// so the navigation machine returns to idle before publication.
					if (getActiveCandidate(ctx) === candidate) {
						ctx.navMachine = { kind: "idle" };
					}
				},
				href: candidate.intent.href,
				historyState: candidate.intent.historyState,
				reason,
				documentUpdate: "apply",
				historyEntry,
				scrollIntent:
					role.kind === "revalidation"
						? undefined
						: decideScrollIntent({
								intent: candidate.intent,
								url,
								restoreScroll:
									role.kind === "traversal"
										? role.restoreScroll
										: undefined,
								isNewDestination,
							}),
				stateFields:
					role.kind === "revalidation"
						? { nav: undefined }
						: {
								nav: undefined,
								lastNavResult: {
									kind: "committed",
									href: candidate.intent.href,
								},
							},
			});
			if (committed.kind === "ok") {
				acceptCandidate(candidate, {
					kind: "committed",
					href: committed.value.href,
				});
			}
			return committed;
		},
	});
	switch (decision.kind) {
		case "committed":
			return { kind: "committed", frame: decision.frame };
		case "cancelled":
			return { kind: "cancelled" };
		case "superseded":
			return { kind: "superseded" };
		case "failure":
			if (decision.stage === "document") {
				reloadDocument(ctx, decision.cause);
			}
			return {
				kind: "failure",
				stage: decision.stage,
				cause: decision.cause,
			};
	}
}

function toNavResult(end: CandidateEnd): NavResult {
	switch (end.kind) {
		case "committed":
			return { kind: "committed", href: end.frame.href };
		case "superseded":
		case "redirected":
			return { kind: "superseded" };
		case "cancelled":
			return { kind: "cancelled" };
		case "document-nav":
			return { kind: "document-nav", href: end.href };
		case "failure":
			return { kind: "failure", stage: end.stage, cause: end.cause };
	}
}

type ReturnInitiation =
	| { readonly kind: "not-applicable" }
	| { readonly kind: "moving" }
	| { readonly kind: "settled" }
	| { readonly kind: "replaced-frame"; readonly frame: Frame }
	| { readonly kind: "failed"; readonly cause: unknown };

/** Settles a candidate that ended: state, callers, history, and freshness. */
function finishCandidate(
	ctx: ClientCtx,
	candidate: Candidate,
	end: CandidateEnd,
): void {
	const completion = candidate.completion;
	if (completion.kind === "ended") {
		// A successor replaced it, or the client closed it, and settled its callers.
		return;
	}
	const result =
		completion.kind === "accepted" ? completion.result : toNavResult(end);
	candidate.completion = { kind: "ended", result };
	candidate.scope.dispose();
	const role = candidate.role;
	const isTraversal =
		role.kind === "traversal" || role.kind === "redirected-traversal";
	// A commit records its result with its frame, and leaving the document
	// is not a result that the page records. Recording a superseded result
	// could overwrite a successor's synchronous navigation result.
	const shouldRecordResult =
		role.kind !== "revalidation" &&
		(end.kind === "cancelled" || end.kind === "failure");
	const wasActive = getActiveCandidate(ctx) === candidate;
	let returnOutcome: ReturnInitiation = { kind: "not-applicable" };
	if (wasActive) {
		if (end.kind === "cancelled" && isTraversal) {
			returnOutcome = initiateReturnToDepartedEntry(ctx, role);
		} else {
			ctx.navMachine = { kind: "idle" };
		}
		const attempt = getFreshnessAttempt(ctx.freshness);
		if (
			(end.kind === "cancelled" ||
				end.kind === "failure" ||
				end.kind === "superseded") &&
			attempt.phase === "running" &&
			attempt.executor === "navigation"
		) {
			ctx.freshness = abandonAttempt(ctx.freshness).facts;
		}
	}
	if (wasActive || shouldRecordResult) {
		updateState(ctx, (state) => {
			const nextNav = deriveNavSlice({
				machine: ctx.navMachine,
				current: state.nav,
			});
			const nextLastNavResult = shouldRecordResult
				? result
				: state.lastNavResult;
			const nextFrame =
				returnOutcome.kind === "replaced-frame" &&
				state.lifecycle === "ready" &&
				state.frame.revision === returnOutcome.frame.revision
					? returnOutcome.frame
					: state.frame;
			const nextRevalidation = deriveRevalidationSlice({
				facts: ctx.freshness,
				current: state.revalidation,
			});
			if (
				nextNav === state.nav &&
				nextLastNavResult === state.lastNavResult &&
				nextFrame === state.frame &&
				nextRevalidation === state.revalidation
			) {
				return state;
			}
			return state.lifecycle === "ready" && nextFrame !== undefined
				? {
						...state,
						frame: nextFrame,
						nav: nextNav,
						lastNavResult: nextLastNavResult,
						revalidation: nextRevalidation,
					}
				: {
						...state,
						nav: nextNav,
						lastNavResult: nextLastNavResult,
						revalidation: nextRevalidation,
					};
		});
	}
	if (returnOutcome.kind === "failed") {
		reloadDocument(ctx, returnOutcome.cause);
	}
	if (
		end.kind === "failure" &&
		role.kind !== "revalidation" &&
		!isTraversal &&
		end.stage !== "document"
	) {
		// A failure that reloads the document is reported once, by the reload.
		reportFailure(ctx, "nav", end.cause);
	}
	settleFreshness(ctx, { candidate, end });
	if (end.kind === "failure" && isTraversal && end.stage !== "document") {
		reloadDocument(ctx, end.cause);
	}
	settleObservers(completion.observers, result);
	maybeStartAttempt(ctx);
}

/**
 * The number of entries from the arrived entry back to the departed entry.
 * The Navigation API's indexes give it where the browser has that API, also
 * for entries that Vorma did not write; otherwise the entries' positions give
 * it when both are in one epoch. It is undefined when neither gives it.
 */
function measureReturnDistance(
	role: Extract<
		CandidateRole,
		{ readonly kind: "traversal" | "redirected-traversal" }
	>,
): number | undefined {
	const { departed, arrived } = role;
	if (
		departed.navigationIndex !== undefined &&
		arrived.navigationIndex !== undefined
	) {
		const distance = departed.navigationIndex - arrived.navigationIndex;
		return distance === 0 ? undefined : distance;
	}
	if (
		departed.position !== undefined &&
		arrived.position !== undefined &&
		departed.position.epoch === arrived.position.epoch &&
		departed.position.index !== arrived.position.index
	) {
		return departed.position.index - arrived.position.index;
	}
	return undefined;
}

/**
 * Checks whether the browser remains on the history entry where a traversal
 * arrived.
 */
function checkBrowserAtArrivedEntry(arrived: ArrivedEntryLocation): boolean {
	const current = readBrowserHistoryEntry();
	if (current.kind !== "ok" || current.value.entryKey !== arrived.entryKey) {
		return false;
	}
	if (arrived.navigationIndex !== undefined) {
		return (
			current.value.navigationIndex === arrived.navigationIndex &&
			readDestination(new URL(current.value.href)) ===
				readDestination(new URL(arrived.href))
		);
	}
	return current.value.href === arrived.href;
}

/**
 * Returns the browser to the entry that a cancelled traversal left so that
 * the page that the hook kept stays with its own URL. With a known distance
 * while the browser remains on the arrived entry, the browser moves back, and
 * the move completes with a `popstate`. Otherwise, the client replaces the
 * current entry with the retained page and starts a new history epoch.
 * Forward entries remain available, but the client cannot recover the original
 * position without a measurable distance from the arrived entry. A browser
 * that refuses the move or the write reloads the document.
 */
function initiateReturnToDepartedEntry(
	ctx: ClientCtx,
	role: Extract<
		CandidateRole,
		{ readonly kind: "traversal" | "redirected-traversal" }
	>,
): ReturnInitiation {
	const frame = ctx.state.getValue().frame;
	const distance = measureReturnDistance(role);
	if (distance !== undefined && checkBrowserAtArrivedEntry(role.arrived)) {
		ctx.navMachine = {
			kind: "returning-history",
			departed: {
				entryKey: frame?.entryKey,
				position: role.departed.position,
				navigationIndex: role.departed.navigationIndex,
			},
			phase: "initiating",
		};
		const moved = moveHistory(distance);
		if (moved.kind === "error") {
			ctx.navMachine = { kind: "idle" };
			return { kind: "failed", cause: moved.error };
		}
		if (ctx.navMachine.kind === "returning-history") {
			ctx.navMachine = {
				...ctx.navMachine,
				phase: "awaiting-popstate",
			};
			return { kind: "moving" };
		}
		return { kind: "settled" };
	}
	ctx.navMachine = { kind: "idle" };
	if (frame === undefined) {
		return { kind: "settled" };
	}
	const rewritten = writeBrowserHistoryEntry({
		href: frame.href,
		placement: "replace-in-new-epoch",
		userState: frame.historyState,
		entryKey: createRandomId(),
	});
	if (rewritten.kind === "error") {
		return { kind: "failed", cause: rewritten.error };
	}
	const entry = rewritten.value;
	return {
		kind: "replaced-frame",
		frame: {
			...frame,
			entryKey: entry.entryKey,
			entryPosition: entry.position,
			entryNavigationIndex: entry.navigationIndex,
		},
	};
}

function settleFreshness(
	ctx: ClientCtx,
	input: { readonly candidate: Candidate; readonly end: CandidateEnd },
): void {
	const { candidate, end } = input;
	const facts = ctx.freshness;
	const attempt = getFreshnessAttempt(facts);
	switch (end.kind) {
		case "committed": {
			if (end.frame.error !== undefined) {
				if (attempt.phase === "running") {
					applyFreshness(
						ctx,
						failAttempt(facts, {
							kind: "failure",
							reason: "page",
							cause: end.frame.error.error,
						}),
					);
				}
				return;
			}
			applyFreshness(
				ctx,
				satisfyDemand(facts, { sequence: end.frame.readSequence }),
			);
			const leaf = end.frame.entries.at(-1)?.view?.staleTimeMillis;
			if (
				candidate.role.kind !== "revalidation" &&
				ctx.state.getValue().lifecycle === "ready" &&
				ctx.state.getValue().frame === end.frame &&
				candidate.generation === ctx.generation &&
				leaf !== undefined
			) {
				const destination = readDestination(new URL(end.frame.href));
				if (!ctx.retainedOutputs.has(destination)) {
					retainOutputs(ctx, {
						prepared: toPreparedPage(ctx, end.frame),
						ttlMillis: leaf,
					});
				}
			}
			return;
		}
		case "superseded":
			// No candidate carries the attempt when the slot is empty.
			if (getActiveCandidate(ctx) === undefined) {
				applyFreshness(ctx, abandonAttempt(facts));
			}
			return;
		case "redirected":
			applyFreshness(ctx, retireDemand(facts, { kind: "cancelled" }));
			return;
		case "document-nav":
			// Leaving the document ends the demand: no read follows. A refused
			// build fails a revalidation, which leaves the page in place.
			applyFreshness(
				ctx,
				candidate.role.kind === "revalidation" && end.buildSkew
					? failAttempt(facts, {
							kind: "failure",
							reason: "build-skew",
							cause: new Error("The server refused this build."),
						})
					: retireDemand(facts, { kind: "cancelled" }),
			);
			return;
		case "cancelled":
			if (
				candidate.role.kind === "revalidation" &&
				attempt.phase === "running" &&
				attempt.executor === "revalidation"
			) {
				applyFreshness(ctx, failAttempt(facts, { kind: "cancelled" }));
			}
			return;
		case "failure":
			if (
				candidate.role.kind === "revalidation" &&
				attempt.phase === "running" &&
				attempt.executor === "revalidation"
			) {
				applyFreshness(
					ctx,
					failAttempt(facts, {
						kind: "failure",
						reason: "page",
						cause: end.cause,
					}),
				);
			}
			return;
	}
}

/**
 * Advances the generation: the late results of older reads are discarded and
 * the reusable data is retired. A revalidation read of the old generation
 * closes and returns its attempt to the demand, which the caller's next
 * registration starts; a pending navigation restarts through
 * `restartPendingNav`, after the demand that the invalidation registers.
 */
export function advanceGeneration(ctx: ClientCtx): void {
	ctx.generation += 1;
	retirePreparationsAndOutputs(ctx);
	for (const run of Array.from(ctx.endpointRuns)) {
		if (run.endsWithGeneration) {
			run.interrupt("generation-retired");
		}
	}
	if (getActiveCandidate(ctx)?.role.kind === "revalidation") {
		vacateSlot(ctx);
	}
}
