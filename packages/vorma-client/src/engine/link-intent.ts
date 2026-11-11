import { callWithResult } from "primitives-ts/results";
import { getCurrentCtx } from "./current.ts";
import { holdPrefetch } from "./prefetch.ts";
import { checkNavigableOrigin } from "./targets.ts";

/*
The intent that holds a prefetch while a pointer or focus rests on a link,
which does not depend on a renderer.
*/

export type LinkIntent = {
	/** Starts a prefetch hold after the delay, or 100 ms when omitted. */
	readonly start: (
		source: "pointer" | "focus",
		delayMillis: number | undefined,
	) => void;
	/** Records that the pointer or focus left; a pointer leaving during touch input is ignored. */
	readonly end: (source: "pointer" | "focus") => void;
	/** Releases pointer interest when the browser cancels its gesture, including touch. */
	readonly cancelPointer: () => void;
	readonly reset: () => void;
};

export const DEFAULT_INTENT_DELAY_MILLIS = 100;

/** Whether the device's last pointer input was touch. */
let pointerModality: "touch" | "other" = "other";

/** Tracks whether the last pointer input was touch; the returned function detaches. */
export function attachPointerModalityTracking(): () => void {
	function handleTouchStart(): void {
		pointerModality = "touch";
	}
	function handlePointerInput(event: PointerEvent): void {
		if (event.pointerType === "touch") {
			pointerModality = "touch";
		} else if (
			event.pointerType === "mouse" ||
			event.pointerType === "pen"
		) {
			pointerModality = "other";
		}
	}
	window.addEventListener("touchstart", handleTouchStart);
	window.addEventListener("pointerdown", handlePointerInput);
	window.addEventListener("pointermove", handlePointerInput);
	return () => {
		window.removeEventListener("touchstart", handleTouchStart);
		window.removeEventListener("pointerdown", handlePointerInput);
		window.removeEventListener("pointermove", handlePointerInput);
	};
}

/** One link's prefetch intent. `getHref` reads the link's current href at the moment that the hold starts. */
export function createLinkIntent(input: {
	readonly getHref: () => string | undefined;
}): LinkIntent {
	type IntentState =
		| { readonly kind: "idle" }
		| {
				readonly kind: "delayed";
				readonly timer: ReturnType<typeof setTimeout>;
		  }
		| { readonly kind: "starting" }
		| { readonly kind: "holding"; readonly release: () => void };
	let state: IntentState = { kind: "idle" };
	const sources = new Set<"pointer" | "focus">();

	function startHold(pending: IntentState): void {
		if (state !== pending) {
			return;
		}
		const starting = { kind: "starting" } as const;
		state = starting;
		const ctx = getCurrentCtx();
		const href = input.getHref();
		if (state !== starting) {
			return;
		}
		if (ctx === undefined || href === undefined) {
			state = { kind: "idle" };
			return;
		}
		const url = callWithResult(() => new URL(href, window.location.href));
		if (url.kind === "error" || !checkNavigableOrigin(url.value)) {
			state = { kind: "idle" };
			return;
		}
		const release = holdPrefetch(ctx, url.value);
		if (state !== starting) {
			release();
			return;
		}
		state = { kind: "holding", release };
	}

	function releaseHold(): void {
		const previous = state;
		state = { kind: "idle" };
		if (previous.kind === "delayed") {
			clearTimeout(previous.timer);
		} else if (previous.kind === "holding") {
			previous.release();
		}
	}

	function releaseSource(source: "pointer" | "focus"): void {
		sources.delete(source);
		if (sources.size === 0) {
			releaseHold();
		}
	}

	return {
		start: (source, delayMillis) => {
			sources.add(source);
			if (state.kind !== "idle") {
				return;
			}
			const pending: IntentState = {
				kind: "delayed",
				timer: setTimeout(
					() => startHold(pending),
					delayMillis ?? DEFAULT_INTENT_DELAY_MILLIS,
				),
			};
			state = pending;
		},
		end: (source) => {
			if (source === "pointer" && pointerModality === "touch") {
				return;
			}
			releaseSource(source);
		},
		cancelPointer: () => releaseSource("pointer"),
		reset: () => {
			sources.clear();
			releaseHold();
		},
	};
}
