import { callWithResult, type Result } from "primitives-ts/results";
import type { ScrollOutcome } from "./scroll.ts";

/*
Focus after the scroll of a navigation's commit, as a browser's document
navigation sets it. After a scroll to a position, the active element becomes
`body`, and sequential focus navigation starts at the start of the document.
After a scroll to a fragment target, a target that can receive focus receives
it, and a target that cannot becomes the starting point of sequential focus
navigation, with `body` as the active element. Without a scroll, focus stays.
None of these steps scrolls.

The platform sets the starting point of sequential focus navigation only
through focus, so an element that cannot receive focus receives a
`tabindex` of -1 for one focus and loses it again before the reset returns.
An element that cannot receive focus even then, such as one that is not
rendered, does not become the starting point, and sequential focus navigation
starts at the start of the document.
*/

/** Focuses the element without scrolling and reports whether it became the active element. */
function focusWithoutScroll(element: HTMLElement | SVGElement): boolean {
	element.focus({ preventScroll: true });
	return document.activeElement === element;
}

/**
 * Makes the element the starting point of sequential focus navigation and
 * leaves `body` as the active element. It reports whether the element took
 * focus for that.
 */
function moveStartingPoint(element: HTMLElement | SVGElement): boolean {
	if (element.hasAttribute("tabindex")) {
		const focused = focusWithoutScroll(element);
		element.blur();
		return focused;
	}
	element.setAttribute("tabindex", "-1");
	const focused = focusWithoutScroll(element);
	element.removeAttribute("tabindex");
	element.blur();
	return focused;
}

/**
 * Moves focus after the scroll of a navigation's commit, as the scroll that
 * the commit performed decides: a scroll to a position resets focus to the
 * document, a scroll to a fragment target focuses the target or makes it the
 * starting point, and no scroll leaves focus where it is.
 */
export function moveFocusAfterScroll(
	outcome: ScrollOutcome,
): Result<void, unknown> {
	return callWithResult(() => {
		if (outcome.kind === "none") {
			return;
		}
		if (outcome.kind === "fragment-target") {
			const { target } = outcome;
			if (
				(target instanceof HTMLElement ||
					target instanceof SVGElement) &&
				(focusWithoutScroll(target) || moveStartingPoint(target))
			) {
				return;
			}
		}
		moveStartingPoint(document.body);
	});
}
