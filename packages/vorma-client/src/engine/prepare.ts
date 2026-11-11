import { awaitWithAbort } from "primitives-ts/async-work/cancellation";
import { type ResourceScope } from "primitives-ts/resource-scopes";
import { createSuccessResult } from "primitives-ts/results";
import { preloadModules } from "../browser/module-preload.ts";
import { preloadStaticPublicAssets } from "../browser/static-public-assets.ts";
import {
	prepareStylesheets,
	type PageStylesheets,
} from "../browser/stylesheets.ts";
import type { AnyPatternMatch } from "../generated-types.ts";
import type { RedirectStatus } from "../protocol/constants.ts";
import type {
	DocumentAttrs,
	HeadSections,
	PageServerFailure,
	RouteRecord,
} from "../protocol/wire.ts";
import type { BuildSkew, RequestKind } from "../results.ts";
import type { RendererNeutralViewDef } from "../views.ts";
import { getBootMetadata } from "./boot-metadata.ts";
import {
	createConfirmation,
	startClientLoader,
	toKnownMatch,
	type RunningLoader,
} from "./client-loaders.ts";
import type { FrameError, RouteEntry } from "./core-state.ts";
import type { ClientCtx } from "./ctx.ts";
import { reportFailure } from "./events.ts";
import { loadView } from "./modules.ts";
import { toBrowserPageError, toServerPageError } from "./page-error.ts";
import { readPage } from "./read-page.ts";
import { readDestination } from "./targets.ts";

/*
Preparation: everything that a page needs before it can replace the committed
page. A preparation reads the page (or takes it from the document), loads its
modules, runs its client loaders, and waits for its stylesheets. It changes
neither the history nor the state. In the document's head, it adds only
elements that do not change what the document shows: module preloads, preloads
of static public assets, and the destination's stylesheets, which do not apply
until the commit applies them.
*/

export type PreparedPage = {
	readonly kind: "prepared";
	/** The path and search of the request. */
	readonly destination: string;
	readonly entries: ReadonlyArray<RouteEntry>;
	readonly error: FrameError | undefined;
	readonly head: HeadSections;
	readonly documentAttrs: DocumentAttrs;
	/** The page's managed stylesheets, which the commit applies. */
	readonly stylesheets: PageStylesheets;
	/** Orders the read against demands. */
	readonly readSequence: number;
	readonly generation: number;
	/**
	 * The time at which the preparation completed, after its read, its modules,
	 * and its client loaders. A retained copy keeps it.
	 */
	readonly preparedAt: number;
	/** The history state that the client loaders ran with. */
	readonly historyState: unknown;
	/** The records, so that the client loaders can run again over the same server answer. */
	readonly records: ReadonlyArray<RouteRecord>;
	readonly serverFailure: PageServerFailure | undefined;
};

export type PreparationOutcome =
	| PreparedPage
	| {
			readonly kind: "redirect";
			readonly href: string;
			readonly status: RedirectStatus;
	  }
	| { readonly kind: "build-skew"; readonly skew: BuildSkew }
	/** The response is not Vorma's. */
	| { readonly kind: "document" }
	| {
			readonly kind: "failure";
			readonly stage: "request" | "modules";
			readonly cause: unknown;
	  }
	| { readonly kind: "aborted" };

/** A preparation in flight, with the scope that holds its work. */
export type Preparation = {
	readonly destination: string;
	readonly scope: ResourceScope;
	readonly startSequence: number;
	readonly outcome: Promise<PreparationOutcome>;
	/** Resolves true when the page, its modules, and its client loader starts exist, and false if the preparation ends without them. */
	readonly ready: Promise<boolean>;
};

type PrepareInput = {
	readonly url: URL;
	readonly requestKind: Extract<
		RequestKind,
		"nav" | "prefetch" | "revalidation"
	>;
	readonly historyState: unknown;
	/** Whether client loaders may start before the server confirms the destination. */
	readonly mayPrestartLoaders: boolean;
};

/** Starts a preparation with a child scope under the application's scope. */
export function startPreparation(
	ctx: ClientCtx,
	input: PrepareInput,
): Preparation {
	const scope = ctx.scope.createChild();
	const ready = Promise.withResolvers<boolean>();
	const startSequence = ctx.lastSequence;
	const outcome = preparePage(ctx, {
		...input,
		scope,
		markReady: ready.resolve,
	}).then((result) => {
		ready.resolve(false);
		return result;
	});
	return {
		destination: readDestination(input.url),
		scope,
		startSequence,
		outcome,
		ready: ready.promise,
	};
}

async function preparePage(
	ctx: ClientCtx,
	input: PrepareInput & {
		readonly scope: ResourceScope;
		readonly markReady: (ready: boolean) => void;
	},
): Promise<PreparationOutcome> {
	const { scope, url } = input;
	const readSequence = ++ctx.lastSequence;
	const generation = ctx.generation;
	const confirmation = createConfirmation();
	// The read starts first, so that nothing delays the request.
	const read = readPage(ctx, {
		url,
		requestKind: input.requestKind,
		scope,
	});
	const prestarted = new Map<number, RunningLoader>();
	if (input.mayPrestartLoaders) {
		const eligible = ctx.loaders.findEligibleMatches(url);
		const known = eligible.map((entry) => entry.match);
		for (const [index, entry] of eligible.entries()) {
			if (entry.view?.clientLoader !== undefined) {
				prestarted.set(
					index,
					startClientLoader({
						view: entry.view,
						positionIndex: index,
						href: url.href,
						historyState: input.historyState,
						match: entry.match,
						known,
						confirmation,
						parentScope: scope,
						outputParentScope: ctx.outputResourceScope,
					}),
				);
			}
		}
	}
	const settled = await awaitWithAbort({
		signal: scope.signal,
		operationFn: () => read,
	});
	if (settled.kind === "error") {
		return settled.error.kind === "aborted"
			? { kind: "aborted" }
			: { kind: "failure", stage: "request", cause: settled.error.cause };
	}
	const result = settled.value;
	switch (result.kind) {
		case "payload":
			break;
		case "aborted":
			return { kind: "aborted" };
		case "failure":
			return { kind: "failure", stage: "request", cause: result.cause };
		default:
			return result;
	}
	const { payload } = result;
	const stylesheets = prepareStylesheets(payload.cssBundles, scope);
	preloadStaticPublicAssets(payload.staticPublicAssetPreloads, scope);
	if (!getBootMetadata(ctx).devMode) {
		preloadModules(payload.modulePreloadUrls);
	}
	const completed = await completePages(ctx, {
		scope,
		url,
		historyState: input.historyState,
		records: payload.routes,
		serverFailure: payload.failure,
		prestarted,
		confirmation,
		views: undefined,
		stylesheetsReady: stylesheets.ready,
		onReady: () => input.markReady(true),
	});
	if (completed.kind !== "completed") {
		return completed;
	}
	return {
		kind: "prepared",
		destination: readDestination(url),
		entries: completed.entries,
		error: completed.error,
		head: payload.head,
		documentAttrs: payload.documentAttrs,
		stylesheets: stylesheets.stylesheets,
		readSequence,
		generation,
		preparedAt: Date.now(),
		historyState: input.historyState,
		records: payload.routes,
		serverFailure: payload.failure,
	};
}

type CompletedPages =
	| {
			readonly kind: "completed";
			readonly entries: ReadonlyArray<RouteEntry>;
			readonly error: FrameError | undefined;
	  }
	| {
			readonly kind: "failure";
			readonly stage: "modules";
			readonly cause: unknown;
	  }
	| { readonly kind: "aborted" };

/**
 * Loads each position's module, runs the client loaders, and builds the page's
 * entries and error. A position's client loader starts as soon as its own
 * module has loaded. The earliest failing client loader is the page error and
 * stops the loaders after its position. `views` supplies already-loaded views
 * (to run the loaders again over the same server answer).
 */
async function completePages(
	ctx: ClientCtx,
	input: {
		readonly scope: ResourceScope;
		readonly url: URL;
		readonly historyState: unknown;
		readonly records: ReadonlyArray<RouteRecord>;
		readonly serverFailure: PageServerFailure | undefined;
		readonly prestarted: Map<number, RunningLoader>;
		readonly confirmation: ReturnType<typeof createConfirmation>;
		readonly views:
			| ReadonlyArray<RendererNeutralViewDef | undefined>
			| undefined;
		readonly stylesheetsReady: Promise<void>;
		readonly onReady?: () => void;
	},
): Promise<CompletedPages> {
	const { scope, records, prestarted } = input;
	input.confirmation.confirm(records);
	// A prestarted loader stops if the server did not confirm its position.
	for (const [index, loader] of prestarted) {
		if (records[index]?.pattern !== loader.pattern) {
			loader.scope.dispose();
			prestarted.delete(index);
		}
	}
	const known = records.map(toKnownMatch);
	const loaders = new Map<number, RunningLoader>();
	let boundary = Number.POSITIVE_INFINITY;
	function lowerBoundary(index: number): void {
		if (index >= boundary) {
			return;
		}
		boundary = index;
		for (const [other, loader] of loaders) {
			if (other > index) {
				loader.scope.dispose();
			}
		}
	}
	function trackLoader(index: number, loader: RunningLoader): void {
		loaders.set(index, loader);
		void loader.outcome.then((outcome) => {
			if (outcome.kind === "failure") {
				lowerBoundary(index);
			}
		});
	}
	for (const [index, loader] of prestarted) {
		trackLoader(index, loader);
	}
	/** Starts a position's loader once its module has loaded, unless an earlier loader already failed. */
	function startLearnedLoader(
		index: number,
		view: RendererNeutralViewDef | undefined,
	): void {
		const record = records[index];
		if (
			record === undefined ||
			view?.clientLoader === undefined ||
			loaders.has(index) ||
			index > boundary ||
			scope.signal.aborted
		) {
			return;
		}
		trackLoader(
			index,
			startClientLoader({
				view,
				positionIndex: index,
				href: input.url.href,
				historyState: input.historyState,
				match: toKnownMatch(record),
				known,
				confirmation: input.confirmation,
				parentScope: scope,
				outputParentScope: ctx.outputResourceScope,
			}),
		);
	}
	const loaded = await awaitWithAbort({
		signal: scope.signal,
		operationFn: () => {
			return Promise.all(
				records.map(async (record, index) => {
					const result =
						input.views !== undefined
							? createSuccessResult(input.views[index])
							: record.moduleUrl === null
								? createSuccessResult(undefined)
								: await loadView({
										url: record.moduleUrl,
										pattern: record.pattern,
									});
					if (result.kind === "ok") {
						startLearnedLoader(index, result.value);
					}
					return result;
				}),
			);
		},
	});
	if (loaded.kind === "error") {
		return loaded.error.kind === "aborted"
			? { kind: "aborted" }
			: { kind: "failure", stage: "modules", cause: loaded.error.cause };
	}
	const views: Array<RendererNeutralViewDef | undefined> = [];
	for (const result of loaded.value) {
		if (result.kind === "error") {
			return { kind: "failure", stage: "modules", cause: result.error };
		}
		views.push(result.value);
	}
	ctx.loaders.recordConfirmedChain({
		url: input.url,
		positions: records.map((record, index) => {
			return { record, view: views[index] };
		}),
	});
	input.onReady?.();
	const settled = await awaitWithAbort({
		signal: scope.signal,
		operationFn: () => {
			return Promise.all([
				input.stylesheetsReady,
				...[...loaders.entries()].map(async ([index, loader]) => {
					return [index, await loader.outcome] as const;
				}),
			]);
		},
	});
	if (settled.kind === "error") {
		return settled.error.kind === "aborted"
			? { kind: "aborted" }
			: { kind: "failure", stage: "modules", cause: settled.error.cause };
	}
	const outcomes = new Map(
		settled.value.slice(1) as ReadonlyArray<
			readonly [number, Awaited<RunningLoader["outcome"]>]
		>,
	);
	let loaderFailure: FrameError | undefined;
	for (const [index, outcome] of [...outcomes].sort(
		([left], [right]) => left - right,
	)) {
		if (outcome.kind === "failure") {
			loaderFailure = {
				positionIndex: index,
				pattern: records[index]?.pattern,
				error: toBrowserPageError({
					kind: "client-loader-failure",
					pattern: records[index]?.pattern ?? "",
					cause: outcome.cause,
				}),
			};
			reportFailure(ctx, "client-loader", outcome.cause);
			break;
		}
	}
	const entries = records.map((record, index): RouteEntry => {
		const outcome = outcomes.get(index);
		const loader = loaders.get(index);
		const clientOutput =
			outcome?.kind === "success" && !loader?.scope.signal.aborted
				? outcome.value
				: undefined;
		const searchInput =
			record.searchInput.kind === "present"
				? record.searchInput.value
				: undefined;
		const serverOutput =
			record.loaderOutput.kind === "present"
				? record.loaderOutput.value
				: undefined;
		return {
			pattern: record.pattern,
			params: record.params,
			splatValues: record.splatValues,
			input: searchInput,
			view: views[index],
			serverOutput,
			clientOutput,
			clientOutputResources:
				outcome?.kind === "success" && !loader?.scope.signal.aborted
					? loader?.outputResources
					: undefined,
			match: {
				pattern: record.pattern,
				params: record.params,
				splatValues: record.splatValues,
				input: searchInput,
				serverOutput,
				clientOutput,
			} as AnyPatternMatch,
		};
	});
	const serverError: FrameError | undefined =
		input.serverFailure === undefined
			? undefined
			: {
					positionIndex: input.serverFailure.positionIndex,
					pattern: undefined,
					error: toServerPageError(input.serverFailure),
				};
	return {
		kind: "completed",
		entries,
		error: loaderFailure ?? serverError,
	};
}

/**
 * Runs the client loaders of a prepared page again, over the same server
 * answer, with a different history state.
 */
export async function rerunClientLoaders(
	ctx: ClientCtx,
	input: {
		readonly prepared: PreparedPage;
		readonly url: URL;
		readonly historyState: unknown;
		readonly scope: ResourceScope;
	},
): Promise<PreparationOutcome> {
	const completed = await completePages(ctx, {
		scope: input.scope,
		url: input.url,
		historyState: input.historyState,
		records: input.prepared.records,
		serverFailure: input.prepared.serverFailure,
		prestarted: new Map(),
		confirmation: createConfirmation(),
		views: input.prepared.entries.map((entry) => entry.view),
		stylesheetsReady: Promise.resolve(),
	});
	if (completed.kind !== "completed") {
		return completed;
	}
	return {
		...input.prepared,
		entries: completed.entries,
		error: completed.error,
		historyState: input.historyState,
	};
}

/** Prepares the page that the initial document already shows, from its bootstrap payload. */
export async function prepareBootPage(
	ctx: ClientCtx,
	input: {
		readonly records: ReadonlyArray<RouteRecord>;
		readonly serverFailure: PageServerFailure | undefined;
		readonly url: URL;
		readonly historyState: unknown;
		readonly head: HeadSections;
		readonly documentAttrs: DocumentAttrs;
		readonly stylesheets: PageStylesheets;
		readonly scope: ResourceScope;
	},
): Promise<PreparationOutcome> {
	const readSequence = ++ctx.lastSequence;
	const completed = await completePages(ctx, {
		scope: input.scope,
		url: input.url,
		historyState: input.historyState,
		records: input.records,
		serverFailure: input.serverFailure,
		prestarted: new Map(),
		confirmation: createConfirmation(),
		views: undefined,
		stylesheetsReady: Promise.resolve(),
	});
	if (completed.kind !== "completed") {
		return completed;
	}
	return {
		kind: "prepared",
		destination: readDestination(input.url),
		entries: completed.entries,
		error: completed.error,
		head: input.head,
		documentAttrs: input.documentAttrs,
		stylesheets: input.stylesheets,
		readSequence,
		generation: ctx.generation,
		preparedAt: Date.now(),
		historyState: input.historyState,
		records: input.records,
		serverFailure: input.serverFailure,
	};
}
