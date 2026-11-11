import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "primitives-ts/results";
import { compareValuesForStructuralEquality } from "primitives-ts/value-comparison";
import { applyDocumentAttrs } from "../browser/document-attrs.ts";
import { applyHead } from "../browser/head.ts";
import {
	readBrowserHistoryEntry,
	writeBrowserHistoryEntry,
	type BrowserHistoryEntry,
	type HistoryStateError,
} from "../browser/history.ts";
import type { ScrollTarget } from "../browser/scroll.ts";
import { applyStylesheets } from "../browser/stylesheets.ts";
import { createRandomId } from "../foundation/random-id.ts";
import type { AnyPatternMatch } from "../generated-types.ts";
import type { CommitReason } from "../results.ts";
import { retainClientOutputs } from "./client-output-resources.ts";
import type { CoreSlices, Frame, RouteEntry } from "./core-state.ts";
import type { ClientCtx } from "./ctx.ts";
import type { PreparedPage } from "./prepare.ts";
import { updateState } from "./state.ts";

export type CommitFailure = {
	readonly stage: "commit" | "document";
	readonly cause: unknown;
};

/** How a commit writes history. */
export type HistoryEntryWrite =
	/** Adds an entry, or replaces the current one. */
	| { readonly kind: "write"; readonly placement: "push" | "replace" }
	/** Takes the entry that the browser is already on, and writes Vorma's fields to it when it has none. */
	| { readonly kind: "adopt" }
	/** Takes the entry that the document boots on, keeps its key, and starts a new epoch for its position. */
	| { readonly kind: "boot" }
	/** Keeps the committed frame's entry, as a hot update does. */
	| { readonly kind: "keep"; readonly entry: BrowserHistoryEntry };

type CommitInput = {
	readonly prepared: PreparedPage;
	/** The destination, including its hash. */
	readonly href: string;
	readonly historyState: unknown;
	readonly reason: CommitReason;
	/** `skip` leaves the document alone: the server already rendered its head, attributes, and stylesheets. */
	readonly documentUpdate: "apply" | "skip";
	readonly historyEntry: HistoryEntryWrite;
	readonly scrollIntent: ScrollTarget | undefined;
	/** Fields that change in the same update that sets the frame. */
	readonly stateFields: Partial<CoreSlices> & {
		readonly lifecycle?: "ready" | "boot-failed";
	};
	/** Finalizes operation ownership before synchronous frame and state subscribers run. */
	readonly beforePublishFn?: () => void;
};

/** Keeps the previous value at a position while it equals the next value. */
function reuseEntry(entries: {
	readonly previous: RouteEntry;
	readonly next: RouteEntry;
}): RouteEntry {
	const { previous, next } = entries;
	if (previous.pattern !== next.pattern) {
		return next;
	}
	const params = compareValuesForStructuralEquality({
		left: previous.params,
		right: next.params,
	})
		? previous.params
		: next.params;
	const splatValues = compareValuesForStructuralEquality({
		left: previous.splatValues,
		right: next.splatValues,
	})
		? previous.splatValues
		: next.splatValues;
	const input = compareValuesForStructuralEquality({
		left: previous.input,
		right: next.input,
	})
		? previous.input
		: next.input;
	const serverOutput = compareValuesForStructuralEquality({
		left: previous.serverOutput,
		right: next.serverOutput,
	})
		? previous.serverOutput
		: next.serverOutput;
	const resourceIdentityMatches =
		previous.clientOutputResources === next.clientOutputResources;
	const canReuseOutput =
		resourceIdentityMatches ||
		(!previous.clientOutputResources?.used &&
			!next.clientOutputResources?.used);
	const clientOutput =
		canReuseOutput &&
		compareValuesForStructuralEquality({
			left: previous.clientOutput,
			right: next.clientOutput,
		})
			? previous.clientOutput
			: next.clientOutput;
	if (
		previous.view === next.view &&
		params === previous.params &&
		splatValues === previous.splatValues &&
		input === previous.input &&
		serverOutput === previous.serverOutput &&
		clientOutput === previous.clientOutput &&
		canReuseOutput
	) {
		return previous;
	}
	return {
		pattern: next.pattern,
		params,
		splatValues,
		input,
		view: next.view,
		serverOutput,
		clientOutput,
		clientOutputResources: next.clientOutputResources,
		match: {
			pattern: next.pattern,
			params,
			splatValues,
			input,
			serverOutput,
			clientOutput,
		} as AnyPatternMatch,
	};
}

/** Writes or takes the history entry for a commit. */
function writeHistoryEntry(input: {
	readonly href: string;
	readonly historyState: unknown;
	readonly historyEntry: HistoryEntryWrite;
}): Result<BrowserHistoryEntry, HistoryStateError> {
	const { historyEntry } = input;
	if (historyEntry.kind === "keep") {
		return createSuccessResult(historyEntry.entry);
	}
	if (historyEntry.kind === "write") {
		return writeBrowserHistoryEntry({
			href: input.href,
			placement: historyEntry.placement,
			userState: input.historyState,
			entryKey: createRandomId(),
		});
	}
	const read = readBrowserHistoryEntry();
	if (read.kind === "error") {
		return read;
	}
	const entry = read.value;
	if (historyEntry.kind === "boot") {
		return writeBrowserHistoryEntry({
			href: entry.href,
			placement: "replace-in-new-epoch",
			userState: entry.userState,
			entryKey: entry.entryKey ?? createRandomId(),
		});
	}
	return entry.entryKey === undefined
		? writeBrowserHistoryEntry({
				href: entry.href,
				placement: "replace",
				userState: entry.userState,
				entryKey: createRandomId(),
			})
		: read;
}

/**
 * The one synchronous commit: write history, apply the head, document
 * attributes, and stylesheets, set the frame and let the renderer update, and
 * publish the state. A failure to apply the document leaves no committed
 * frame; a failure of the renderer leaves the committed frame, because the
 * browser has moved to it. Either way, the caller reloads the document.
 */
export function commitFrame(
	ctx: ClientCtx,
	input: CommitInput,
): Result<Frame, CommitFailure> {
	const previous = ctx.state.getValue().frame;
	const written = writeHistoryEntry(input);
	if (written.kind === "error") {
		return createErrorResult({ stage: "commit", cause: written.error });
	}
	const entry = written.value;
	const { prepared } = input;
	if (input.documentUpdate === "apply") {
		const head = applyHead(prepared.head);
		if (head.kind === "error") {
			return createErrorResult({ stage: "document", cause: head.error });
		}
		const attrs = applyDocumentAttrs({
			previous: previous?.documentAttrs ?? prepared.documentAttrs,
			next: prepared.documentAttrs,
		});
		if (attrs.kind === "error") {
			return createErrorResult({ stage: "document", cause: attrs.error });
		}
		// Each prepared stylesheet applies here, in the same task as the
		// renderer's DOM change below.
		const stylesheets = applyStylesheets(prepared.stylesheets);
		if (stylesheets.kind === "error") {
			return createErrorResult({
				stage: "document",
				cause: new Error(
					`The document has no prepared stylesheet for ${stylesheets.error.bundles.map((bundle) => JSON.stringify(bundle)).join(", ")}.`,
					{ cause: stylesheets.error },
				),
			});
		}
	}
	const entries = prepared.entries.map((entry, index) => {
		const earlier = previous?.entries[index];
		return earlier === undefined
			? entry
			: reuseEntry({ previous: earlier, next: entry });
	});
	const takesBrowserEntry =
		input.historyEntry.kind === "adopt" ||
		input.historyEntry.kind === "boot";
	const frame: Frame = {
		revision: ++ctx.frameRevision,
		href: takesBrowserEntry ? entry.href : input.href,
		historyState: takesBrowserEntry ? entry.userState : input.historyState,
		entryKey: entry.entryKey,
		entryPosition: entry.position,
		entryNavigationIndex: entry.navigationIndex,
		entries,
		error: prepared.error,
		head: prepared.head,
		documentAttrs: prepared.documentAttrs,
		stylesheets: prepared.stylesheets,
		commitReason: input.reason,
		preparedAt: prepared.preparedAt,
		readSequence: prepared.readSequence,
		scrollIntent: input.scrollIntent,
	};
	type PublicationState =
		| { readonly kind: "pending"; readonly releaseNextOutputs: () => void }
		| {
				readonly kind: "applied";
				readonly releasePreviousOutputs: () => void;
		  }
		| { readonly kind: "finished" };
	let publication: PublicationState = {
		kind: "pending",
		releaseNextOutputs: retainClientOutputs(entries),
	};
	function applyFrame(): void {
		if (publication.kind !== "pending") {
			return;
		}
		const pending = publication;
		publication = {
			kind: "applied",
			releasePreviousOutputs: ctx.releaseFrameOutputs,
		};
		ctx.releaseFrameOutputs = pending.releaseNextOutputs;
		ctx.scrollApplication =
			frame.scrollIntent === undefined
				? { kind: "unavailable" }
				: { kind: "pending", revision: frame.revision };
		input.beforePublishFn?.();
		updateState(ctx, (state) => {
			const lifecycle = input.stateFields.lifecycle ?? state.lifecycle;
			if (lifecycle === "pre-boot" || lifecycle === "booting") {
				throw new Error(
					"A boot commit must select its ready or failed lifecycle.",
				);
			}
			return { ...state, ...input.stateFields, lifecycle, frame };
		});
	}
	function finishPublication(): void {
		const previous = publication;
		publication = { kind: "finished" };
		if (previous.kind === "applied") {
			previous.releasePreviousOutputs();
		}
	}
	const flushed = callWithResult(() => ctx.renderer.flushSync(applyFrame));
	// History and document application precede renderer execution, so this frame
	// must become authoritative even when the renderer fails before calling apply.
	const applied = callWithResult(applyFrame);
	finishPublication();
	if (flushed.kind === "error" || applied.kind === "error") {
		return createErrorResult({
			stage: "document",
			cause:
				flushed.kind === "error"
					? flushed.error
					: applied.kind === "error"
						? applied.error
						: undefined,
		});
	}
	return createSuccessResult(frame);
}
