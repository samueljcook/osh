import { callWithResult, type Result } from "primitives-ts/results";
import type { DocumentAttrs } from "../protocol/wire.ts";

/*
Vorma manages the attributes of `html`, `head`, and `body` that the server
resolves for the current page. A commit sets the destination's attributes and
removes those that the previous page set and the destination does not, so
attributes that other code sets stay untouched.
*/

/** Applies the destination's attributes; `previous` holds the attributes that Vorma set for the committed page. */
export function applyDocumentAttrs(input: {
	readonly previous: DocumentAttrs;
	readonly next: DocumentAttrs;
}): Result<void, unknown> {
	return callWithResult(() => {
		const elements = [
			[document.documentElement, "html"],
			[document.head, "head"],
			[document.body, "body"],
		] as const;
		for (const [element, section] of elements) {
			const previous = input.previous[section];
			const next = input.next[section];
			for (const name of Object.keys(previous)) {
				if (!Object.hasOwn(next, name)) {
					element.removeAttribute(name);
				}
			}
			for (const [name, value] of Object.entries(next)) {
				if (element.getAttribute(name) !== value) {
					element.setAttribute(name, value);
				}
			}
		}
	});
}
