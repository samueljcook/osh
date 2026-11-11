import { callWithResult, type Result } from "primitives-ts/results";

/** A target scroll: a coordinate pair, or a hash that identifies an element to scroll into view. */
export type ScrollTarget =
	| { readonly x: number; readonly y: number }
	| { readonly hash: string; readonly scrollToTopIfMissing?: boolean };

export type ScrollPosition = { readonly x: number; readonly y: number };

export function readScrollPosition(): ScrollPosition {
	return { x: window.scrollX, y: window.scrollY };
}

export function normalizeHash(hash: string): string {
	const raw = hash.startsWith("#") ? hash.slice(1) : hash;
	if (raw.length === 0) {
		return raw;
	}
	const decoded = callWithResult(() => decodeURIComponent(raw));
	return decoded.kind === "ok" ? decoded.value : raw;
}

function findHashElement(hash: string): Element | undefined {
	return (
		document.getElementById(hash) ??
		Array.from(document.getElementsByName(hash)).find((element) => {
			return element instanceof HTMLAnchorElement;
		})
	);
}

/**
 * The element that a URL hash identifies: the element with that ID, else an
 * anchor with that name, first for the hash as written and then for the
 * decoded hash.
 */
function findFragmentTarget(hash: string): Element | undefined {
	const raw = hash.startsWith("#") ? hash.slice(1) : hash;
	const decoded = normalizeHash(hash);
	return (
		findHashElement(raw) ??
		(raw === decoded ? undefined : findHashElement(decoded))
	);
}

/**
 * The scroll that `applyScroll` performed: to a fragment target, to a
 * position (the top, or a position that a traversal restores), or none, for a
 * hash without an element that does not fall back to the top.
 */
export type ScrollOutcome =
	| { readonly kind: "fragment-target"; readonly target: Element }
	| { readonly kind: "position" }
	| { readonly kind: "none" };

/** Applies the target through the browser's scroll calls, the one platform boundary here, and reports the scroll that it performed. */
export function applyScroll(
	target: ScrollTarget,
): Result<ScrollOutcome, unknown> {
	return callWithResult((): ScrollOutcome => {
		if ("hash" in target) {
			const decoded = normalizeHash(target.hash);
			const element = findFragmentTarget(target.hash);
			if (element !== undefined) {
				element.scrollIntoView();
				return { kind: "fragment-target", target: element };
			}
			if (
				decoded.length === 0 ||
				decoded.toLowerCase() === "top" ||
				target.scrollToTopIfMissing === true
			) {
				window.scrollTo(0, 0);
				return { kind: "position" };
			}
			return { kind: "none" };
		}
		window.scrollTo(target.x, target.y);
		return { kind: "position" };
	});
}
