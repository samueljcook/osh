import type { DebounceConfigError } from "../../async-work/debounce/index.ts";
import type { Result } from "../../results/index.ts";
import { subscribeToBrowserEvent } from "../event-subscription.ts";
/**
 * Subscribes to visibility changes on the supplied browser document, including
 * hidden and visible states. There is no initial notification. Debounced
 * delivery reports the latest event's state; removal cancels pending delivery.
 * The listener and its error handler must be synchronous. If the document's
 * `removeEventListener` throws, the returned remover finishes its cleanup and
 * then throws an AggregateError.
 */
export function subscribeToDocumentVisibility(input: {
	readonly document: Document;
	readonly timing:
		| { readonly kind: "immediate" }
		| { readonly kind: "debounced"; readonly delayMillis: number };
	readonly listener: (visibilityState: DocumentVisibilityState) => void;
	readonly onListenerErrorFn: (cause: unknown) => void;
}): Result<() => void, DebounceConfigError> {
	const targetDocument = input.document;
	return subscribeToBrowserEvent({
		...input,
		target: targetDocument,
		eventName: "visibilitychange",
		readEventValueFn: () => targetDocument.visibilityState,
	});
}
