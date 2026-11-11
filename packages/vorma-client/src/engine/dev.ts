import { type ResourceScope } from "primitives-ts/resource-scopes";
import { compareValuesForStructuralEquality } from "primitives-ts/value-comparison";
import type { AnyPatternMatch } from "../generated-types.ts";
import { DEV_CONTROLS_PROTOCOL } from "../protocol/constants.ts";
import type { RevalidationResult } from "../results.ts";
import type { ClientLoaderCtx, RendererNeutralViewDef } from "../views.ts";
import { invokeClientLoader, type RunningLoader } from "./client-loaders.ts";
import { commitFrame } from "./commit.ts";
import type { Frame, RouteEntry } from "./core-state.ts";
import type { ClientCtx } from "./ctx.ts";
import { reportFailure } from "./events.ts";
import { enableDevModules } from "./modules.ts";
import { restartActiveCandidate } from "./nav.ts";
import { retirePreparationsAndOutputs } from "./prefetch.ts";
import type { PreparedPage } from "./prepare.ts";
import { registerRevalidationDemand } from "./revalidation.ts";

/*
Development controls: `vorma dev` reaches the client through
`window.__vorma_dev`. A view update replaces the view in the committed page
without a request and runs the loaders of views that opt in. A candidate that
is in flight when an update arrives restarts with a new read, so that its
commit uses the replaced modules. An update never overwrites a newer update or
a navigation: a commit that completes while an update's loaders run receives
the update afterward.
*/

type DevControls = {
	readonly revalidateFn: () => Promise<void>;
	readonly updateModuleFn: (
		url: string,
		module: Readonly<Record<string, unknown>>,
	) => void;
};

type DevWindow = Window & {
	[DEV_CONTROLS_PROTOCOL.windowProperty]?: DevControls;
};

const DEV_CONTROLS_WINDOW_PROPERTY = DEV_CONTROLS_PROTOCOL.windowProperty;

function toMatchSummary(entry: RouteEntry) {
	return {
		pattern: entry.pattern,
		params: entry.params,
		splatValues: entry.splatValues,
		input: entry.input,
	};
}

/** Runs the replaced view's client loader again over the entry's own server answer. */
function rerunLoader(
	ctx: ClientCtx,
	input: {
		readonly frame: Frame;
		readonly entry: RouteEntry;
		readonly view: RendererNeutralViewDef;
		readonly scope: ResourceScope;
	},
): RunningLoader | undefined {
	const { frame, entry, view } = input;
	const { clientLoader } = view;
	if (clientLoader === undefined) {
		return undefined;
	}
	const known = frame.entries.map(toMatchSummary);
	const clientLoaderCtx = {
		href: frame.href,
		historyState: frame.historyState,
		...toMatchSummary(entry),
		knownPatternMatches: known,
		serverConfirmedPatternMatches: Promise.resolve(known),
		serverOutput: Promise.resolve(entry.serverOutput),
		getServerOutput: (pattern: string) => {
			const other = frame.entries.find((candidate) => {
				return candidate.pattern === pattern;
			});
			return Promise.resolve(other?.serverOutput);
		},
	} as Omit<ClientLoaderCtx, "signal" | "outputScope">;
	return invokeClientLoader({
		view,
		positionIndex: frame.entries.indexOf(entry),
		context: clientLoaderCtx,
		parentScope: input.scope,
		outputParentScope: ctx.outputResourceScope,
	});
}

/** Installs the controls and returns nothing; the application's scope removes them. */
export function installDevUpdates(ctx: ClientCtx): void {
	const modules = enableDevModules();
	type UpdateState =
		| { readonly kind: "idle" }
		| { readonly kind: "running"; readonly scope: ResourceScope }
		| { readonly kind: "closed" };
	let state: UpdateState = { kind: "idle" };
	const controls: DevControls = {
		revalidateFn: async () => {
			// The demand's waiter reports a failed revalidation.
			await new Promise<RevalidationResult>((resolve) => {
				registerRevalidationDemand(ctx, {
					immediate: false,
					skipWorkIndicator: true,
					waiter: resolve,
				});
			});
		},
		updateModuleFn(url, module) {
			const previous = state;
			if (previous.kind === "closed") {
				return;
			}
			const scope = ctx.scope.createChild();
			const running: UpdateState = { kind: "running", scope };
			state = running;
			if (previous.kind === "running") {
				previous.scope.dispose();
			}
			if (state !== running || scope.signal.aborted) {
				return;
			}
			const replacements = modules.replaceModule({ url, module });
			if (state !== running) {
				return;
			}
			ctx.loaders.replaceViews(replacements);
			retirePreparationsAndOutputs(ctx);
			if (state !== running) {
				return;
			}
			restartActiveCandidate(ctx);
			if (state !== running) {
				return;
			}
			function finish(): void {
				if (state === running) {
					state = { kind: "idle" };
				}
				scope.dispose();
			}
			void applyUpdate(ctx, {
				replacements,
				scope,
				checkCurrentUpdateFn: () => state === running,
			}).then(finish, (cause: unknown) => {
				reportFailure(ctx, "hmr", cause);
				finish();
			});
		},
	};
	(window as DevWindow)[DEV_CONTROLS_WINDOW_PROPERTY] = controls;
	ctx.scope.addCleanup(() => {
		const previous = state;
		state = { kind: "closed" };
		if (previous.kind === "running") {
			previous.scope.dispose();
		}
		if ((window as DevWindow)[DEV_CONTROLS_WINDOW_PROPERTY] === controls) {
			delete (window as DevWindow)[DEV_CONTROLS_WINDOW_PROPERTY];
		}
		modules.close();
	});
}

async function applyUpdate(
	ctx: ClientCtx,
	input: {
		readonly replacements: ReadonlyMap<unknown, unknown>;
		readonly scope: ResourceScope;
		readonly checkCurrentUpdateFn: () => boolean;
	},
): Promise<void> {
	const { replacements, scope } = input;
	const frame = ctx.state.getValue().frame;
	if (frame === undefined) {
		return;
	}
	function selectReplacement(
		entry: RouteEntry,
	): RendererNeutralViewDef | undefined {
		if (entry.view === undefined || !replacements.has(entry.view)) {
			return undefined;
		}
		const view = replacements.get(entry.view) as
			| RendererNeutralViewDef
			| undefined;
		return view !== undefined && view.pattern === entry.pattern
			? view
			: undefined;
	}
	if (
		!frame.entries.some((entry) => selectReplacement(entry) !== undefined)
	) {
		return;
	}
	// The loaders that opted in run over the frame that the update found.
	const reruns = new Map<
		RouteEntry,
		{ readonly value: unknown; readonly loader: RunningLoader }
	>();
	await Promise.all(
		frame.entries.map(async (entry) => {
			const view = selectReplacement(entry);
			if (view?.rerunClientLoaderOnHmr !== true) {
				return;
			}
			const loader = rerunLoader(ctx, { frame, entry, view, scope });
			if (loader === undefined) {
				return;
			}
			const result = await loader.outcome;
			if (result.kind === "failure") {
				reportFailure(ctx, "hmr", result.cause);
			} else if (result.kind === "success") {
				reruns.set(entry, { value: result.value, loader });
			}
		}),
	);
	if (scope.signal.aborted || !input.checkCurrentUpdateFn()) {
		return;
	}
	// A commit can complete while the loaders run; the update applies to it.
	const latest = ctx.state.getValue().frame;
	if (latest === undefined || ctx.state.getValue().lifecycle !== "ready") {
		return;
	}
	const entries = latest.entries.map((entry): RouteEntry => {
		const view = selectReplacement(entry);
		if (view === undefined) {
			return entry;
		}
		const source = frame.entries.find((earlier) => {
			return (
				earlier.pattern === entry.pattern &&
				compareValuesForStructuralEquality({
					left: earlier.params,
					right: entry.params,
				}) &&
				compareValuesForStructuralEquality({
					left: earlier.input,
					right: entry.input,
				}) &&
				compareValuesForStructuralEquality({
					left: earlier.serverOutput,
					right: entry.serverOutput,
				})
			);
		});
		const rerun = source === undefined ? undefined : reruns.get(source);
		const clientOutput =
			rerun === undefined ? entry.clientOutput : rerun.value;
		return {
			...entry,
			view,
			clientOutput,
			clientOutputResources:
				rerun?.loader.outputResources ?? entry.clientOutputResources,
			match: { ...entry.match, clientOutput } as AnyPatternMatch,
		};
	});
	if (entries.every((entry, index) => entry === latest.entries[index])) {
		// A navigation committed a page that the update does not touch.
		return;
	}
	const prepared: PreparedPage = {
		kind: "prepared",
		destination: "",
		entries,
		error: latest.error,
		head: latest.head,
		documentAttrs: latest.documentAttrs,
		stylesheets: latest.stylesheets,
		readSequence: latest.readSequence,
		generation: ctx.generation,
		preparedAt: latest.preparedAt,
		historyState: latest.historyState,
		records: [],
		serverFailure: undefined,
	};
	const committed = commitFrame(ctx, {
		prepared,
		href: latest.href,
		historyState: latest.historyState,
		reason: "hmr",
		documentUpdate: "skip",
		historyEntry: {
			kind: "keep",
			entry: {
				href: latest.href,
				userState: latest.historyState,
				entryKey: latest.entryKey,
				position: latest.entryPosition,
				navigationIndex: latest.entryNavigationIndex,
			},
		},
		scrollIntent: undefined,
		stateFields: {},
	});
	if (committed.kind === "error") {
		reportFailure(ctx, "hmr", committed.error.cause);
	}
}
