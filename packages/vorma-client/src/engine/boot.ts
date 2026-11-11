import { getErrorMsg } from "primitives-ts/error-msgs";
import { callWithResult, callWithResultAsync } from "primitives-ts/results";
import { readDocumentHead } from "../browser/head.ts";
import {
	readBrowserHistoryEntry,
	recallScroll,
	rememberScroll,
} from "../browser/history.ts";
import { readScrollPosition, type ScrollTarget } from "../browser/scroll.ts";
import { readDocumentStylesheets } from "../browser/stylesheets.ts";
import { DOCUMENT_PROTOCOL } from "../protocol/constants.ts";
import { readBootstrapPayload } from "../protocol/wire.ts";
import type { BootResult } from "../results.ts";
import { recordBootMetadata } from "./boot-metadata.ts";
import { commitFrame } from "./commit.ts";
import type { ClientCtx } from "./ctx.ts";
import { reportFailure } from "./events.ts";
import { attachPointerModalityTracking } from "./link-intent.ts";
import { handlePopstate } from "./nav.ts";
import { toBootPageError } from "./page-error.ts";
import { prepareBootPage, type PreparedPage } from "./prepare.ts";
import { mountRenderer } from "./renderer-ownership.ts";
import {
	attachRevalidationTriggers,
	checkRevalidationOptions,
} from "./revalidation.ts";
import { startWorkIndicator } from "./work-indicator.ts";

/*
Boot reads the document that the server rendered, prepares its page from the
bootstrap payload, commits it as the first page, mounts the renderer, and
attaches the browser listeners. The document has an empty root element and no
component HTML, so the client renders every view's component itself and never hydrates.
*/

const ELEMENT_IDS = DOCUMENT_PROTOCOL.elementIds;

/** The first page's scroll: the entry's remembered position, else its hash. */
function selectFirstScroll(
	key: string | undefined,
	url: URL,
): ScrollTarget | undefined {
	const remembered = key === undefined ? undefined : recallScroll(key);
	if (remembered !== undefined) {
		return remembered;
	}
	return url.hash === ""
		? undefined
		: { hash: url.hash, scrollToTopIfMissing: false };
}

type BootHooks = {
	/** Closes everything but the mounted root; a failed boot keeps its screen. */
	readonly teardownFn: (input: { readonly unmount: boolean }) => void;
};

export async function bootClient(
	ctx: ClientCtx,
	hooks: BootHooks,
): Promise<BootResult> {
	const root = document.getElementById(ELEMENT_IDS.root);
	const dataElement = document.getElementById(ELEMENT_IDS.data);
	const url = new URL(window.location.href);
	const position = readBrowserHistoryEntry();
	if (position.kind === "error") {
		reportFailure(ctx, "boot", position.error);
	}
	const entry = position.kind === "ok" ? position.value : undefined;

	async function failBoot(cause: unknown, msg: string): Promise<BootResult> {
		if (ctx.state.getValue().lifecycle === "disposed") {
			return {
				kind: "failure",
				msg: "The client was disposed during boot.",
				cause,
			};
		}
		reportFailure(ctx, "boot", cause);
		const prepared: PreparedPage = {
			kind: "prepared",
			destination: `${url.pathname}${url.search}`,
			entries: [],
			error: {
				positionIndex: 0,
				pattern: undefined,
				error: toBootPageError(cause),
			},
			head: { title: undefined, meta: [], rest: [] },
			documentAttrs: { html: {}, head: {}, body: {} },
			stylesheets: [],
			readSequence: ++ctx.lastSequence,
			generation: ctx.generation,
			preparedAt: Date.now(),
			historyState: undefined,
			records: [],
			serverFailure: undefined,
		};
		// The failed boot's page state is the document's URL, no matches, and the
		// failure as the page error.
		commitFrame(ctx, {
			prepared,
			href: url.href,
			historyState: undefined,
			reason: "boot",
			documentUpdate: "skip",
			historyEntry: {
				kind: "keep",
				entry: {
					href: url.href,
					userState: undefined,
					entryKey: undefined,
					position: undefined,
					navigationIndex: undefined,
				},
			},
			scrollIntent: undefined,
			stateFields: { lifecycle: "boot-failed" },
		});
		if (root !== null) {
			const mounted = mountRenderer(ctx, root);
			if (mounted.kind === "error") {
				console.error(
					"Vorma could not show the boot failure.",
					mounted.error,
				);
			}
		}
		hooks.teardownFn({ unmount: false });
		return { kind: "failure", msg, cause };
	}

	if (root === null) {
		return failBoot(
			new Error(
				`The document has no element with the id ${ELEMENT_IDS.root}.`,
			),
			"The document has no root element.",
		);
	}
	if (dataElement === null) {
		return failBoot(
			new Error(
				`The document has no element with the id ${ELEMENT_IDS.data}.`,
			),
			"The document has no bootstrap data.",
		);
	}
	const parsed = callWithResult(
		() => JSON.parse(dataElement.textContent ?? "") as unknown,
	);
	if (parsed.kind === "error") {
		return failBoot(parsed.error, "The bootstrap data is not JSON.");
	}
	const payload = readBootstrapPayload(parsed.value);
	if (payload.kind === "error") {
		return failBoot(payload.error, "The bootstrap data is not Vorma's.");
	}
	recordBootMetadata(ctx, {
		clientBuildId: payload.value.clientBuildId,
		deploymentId: payload.value.deploymentId,
		devMode: payload.value.devMode,
	});
	// Vorma's production Vite mode excludes development controls regardless of NODE_ENV.
	if (import.meta.env.MODE !== "production" && payload.value.devMode) {
		const dev = await callWithResultAsync(() => import("./dev.ts"));
		if (dev.kind === "error") {
			reportFailure(ctx, "hmr", dev.error);
		} else {
			dev.value.installDevUpdates(ctx);
		}
	}
	const head = readDocumentHead();
	if (head.kind === "error") {
		return failBoot(head.error, head.error.msg);
	}
	const bootScope = ctx.scope.createChild();
	const prepared = await prepareBootPage(ctx, {
		records: payload.value.routes,
		serverFailure: payload.value.failure,
		url,
		historyState: entry?.userState,
		head: head.value,
		documentAttrs: payload.value.documentAttrs,
		stylesheets: readDocumentStylesheets(),
		scope: bootScope,
	});
	if (ctx.state.getValue().lifecycle === "disposed") {
		return {
			kind: "failure",
			msg: "The client was disposed during boot.",
			cause: new Error("The client was disposed during boot."),
		};
	}
	if (prepared.kind !== "prepared") {
		const cause = prepared.kind === "failure" ? prepared.cause : prepared;
		return failBoot(
			cause,
			getErrorMsg(cause) ?? "The first page could not be prepared.",
		);
	}
	const revalidationOptions = checkRevalidationOptions(ctx);
	if (revalidationOptions.kind === "error") {
		return failBoot(
			revalidationOptions.error,
			revalidationOptions.error.message,
		);
	}
	const indicator = startWorkIndicator(ctx);
	if (indicator.kind === "error") {
		return failBoot(
			indicator.error,
			`The work indicator's ${indicator.error.delay} delay must be between 0 and 2,147,483,647 milliseconds.`,
		);
	}
	ctx.scope.addCleanup(indicator.value);
	const committed = commitFrame(ctx, {
		prepared,
		href: url.href,
		historyState: entry?.userState,
		reason: "boot",
		documentUpdate: "skip",
		historyEntry: { kind: "boot" },
		scrollIntent: selectFirstScroll(entry?.entryKey, url),
		stateFields: { lifecycle: "ready" },
	});
	if (committed.kind === "error") {
		return failBoot(
			committed.error.cause,
			"The first page could not be committed.",
		);
	}
	const mounted = mountRenderer(ctx, root);
	if (mounted.kind === "error") {
		return failBoot(
			mounted.error,
			"The renderer could not mount the application.",
		);
	}
	bootScope.dispose();
	attachBrowserListeners(ctx);
	void callWithResultAsync(() => import("./learned-matching.ts")).then(
		(matching) => {
			if (matching.kind === "ok") {
				ctx.loaders.installMatching(matching.value);
			} else {
				reportFailure(ctx, "pattern-matching", matching.error);
			}
		},
	);
	dataElement.remove();
	return { kind: "booted" };
}

/** Attaches the listeners that a booted client owns; the application's scope removes them. */
function attachBrowserListeners(ctx: ClientCtx): void {
	function rememberCommittedScroll(): void {
		const frame = ctx.state.getValue().frame;
		if (frame?.entryKey !== undefined) {
			rememberScroll(frame.entryKey, readScrollPosition());
		}
	}
	function handleBrowserPopstate(): void {
		handlePopstate(ctx);
	}
	const previousScrollRestoration = history.scrollRestoration;
	history.scrollRestoration = "manual";
	window.addEventListener("popstate", handleBrowserPopstate);
	window.addEventListener("pagehide", rememberCommittedScroll);
	const detachModality = attachPointerModalityTracking();
	const detachTriggers = attachRevalidationTriggers(ctx);
	ctx.scope.addCleanup(() => {
		window.removeEventListener("popstate", handleBrowserPopstate);
		window.removeEventListener("pagehide", rememberCommittedScroll);
		detachModality();
		detachTriggers();
		history.scrollRestoration = previousScrollRestoration;
	});
}
