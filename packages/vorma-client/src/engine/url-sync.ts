import { createDebouncedFunction } from "primitives-ts/async-work/debounce";
import { callWithResult } from "primitives-ts/results";
import type { PageTarget } from "../generated-types.ts";
import type { RawTarget } from "../generated-types.ts";
import type { ClientCtx } from "./ctx.ts";
import { requireCtx } from "./current.ts";
import { reportFailure } from "./events.ts";
import { startNav } from "./nav.ts";
import { resolveTargetHref } from "./targets.ts";

/** How a synchronization navigates to each target. */
export type UrlSyncOptions = {
	/** A target waits this long before it navigates; a newer target replaces it. Positive delays must be finite and at most 2,147,483,647 milliseconds. Omitted or nonpositive delays navigate immediately. */
	readonly debounceMillis?: number;
	/** Replaces the history entry; true when omitted. */
	readonly replace?: boolean;
	/**
	 * Scrolls to the top after each synchronized navigation when true. When
	 * omitted, the page keeps its scroll position, unlike `navigate`, because a
	 * synchronized URL follows the state of a page that the user stays on.
	 */
	readonly scrollToTop?: boolean;
	/** Scrolls to a target's hash when true. When omitted, the page keeps its scroll position. */
	readonly scrollToHash?: boolean;
	/** Keeps the synchronized navigations out of the work indicator when true; they count when omitted. */
	readonly skipWorkIndicator?: boolean;
};

export type UrlSync = {
	/** A new target replaces the pending one. A target equal to the committed or pending href starts nothing. */
	readonly setTarget: (target: PageTarget) => void;
	readonly stop: () => void;
};

function checkEquivalentHref(ctx: ClientCtx, href: string): boolean {
	const state = ctx.state.getValue();
	return state.frame?.href === href || state.nav?.href === href;
}

/**
 * Keeps the URL in step with the application's state. A navigation of any other
 * source, including Back and Forward, cancels the pending target, and a page
 * change never makes the last target apply again, so synchronization cannot
 * undo a navigation that the user started.
 */
export function createUrlSync(options: UrlSyncOptions = {}): UrlSync {
	const ctx = requireCtx();
	const debouncedNavigation =
		options.debounceMillis !== undefined && !(options.debounceMillis <= 0)
			? createDebouncedFunction({
					delayMillis: options.debounceMillis,
					operationFn: navigateTo,
				})
			: undefined;
	const scope = ctx.scope.createChild();
	type SyncState =
		| { readonly kind: "idle" }
		| { readonly kind: "pending"; readonly href: string }
		| { readonly kind: "navigating" }
		| { readonly kind: "stopped" };
	let state: SyncState = { kind: "idle" };

	function cancelPending(): void {
		if (debouncedNavigation?.kind === "ok") {
			debouncedNavigation.value.cancelPendingCalls(undefined);
		}
		if (state.kind === "pending") {
			state = { kind: "idle" };
		}
	}

	function handleNavStart(): void {
		if (state.kind !== "navigating") {
			cancelPending();
		}
	}
	ctx.navStartListeners.add(handleNavStart);

	function navigateTo(href: string): void {
		cancelPending();
		if (
			scope.signal.aborted ||
			ctx.state.getValue().lifecycle !== "ready" ||
			checkEquivalentHref(ctx, href)
		) {
			return;
		}
		// While the navigation starts, its own start notification must not
		// cancel the target.
		const navigating: SyncState = { kind: "navigating" };
		state = navigating;
		const started = callWithResult(() => {
			return startNav(ctx, {
				href,
				replace: options.replace ?? true,
				historyState: undefined,
				scrollToTop: options.scrollToTop ?? false,
				scrollToHash: options.scrollToHash ?? false,
				skipWorkIndicator: options.skipWorkIndicator === true,
				source: "navigate",
				read: "shared",
			});
		});
		if (state === navigating) {
			state = { kind: "idle" };
		}
		if (started.kind === "error") {
			reportFailure(ctx, "nav", started.error);
		}
	}

	function stop(): void {
		scope.dispose();
	}
	scope.addCleanup(() => {
		state = { kind: "stopped" };
		cancelPending();
		ctx.navStartListeners.delete(handleNavStart);
	});

	return {
		setTarget: (target) => {
			if (scope.signal.aborted) {
				return;
			}
			const resolved = resolveTargetHref(target as RawTarget);
			if (resolved.kind === "error") {
				reportFailure(ctx, "nav", new Error(resolved.error));
				return;
			}
			const parsed = callWithResult(
				() => new URL(resolved.value, window.location.href),
			);
			if (parsed.kind === "error") {
				reportFailure(ctx, "nav", parsed.error);
				return;
			}
			const href = parsed.value.href;
			if (checkEquivalentHref(ctx, href)) {
				cancelPending();
				return;
			}
			if (state.kind === "pending" && state.href === href) {
				return;
			}
			cancelPending();
			if (debouncedNavigation === undefined) {
				navigateTo(href);
				return;
			}
			if (debouncedNavigation.kind === "error") {
				reportFailure(
					ctx,
					"nav",
					new Error(
						"The URL synchronization debounce delay is outside the supported timer range.",
					),
				);
				return;
			}
			state = { kind: "pending", href };
			void debouncedNavigation.value(href).then((result) => {
				if (result.kind === "error" && result.error.kind === "failed") {
					reportFailure(ctx, "nav", result.error.cause);
				}
			});
		},
		stop,
	};
}
