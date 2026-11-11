import type { DebounceConfigError } from "../../async-work/debounce/index.ts";
import type { Result } from "../../results/index.ts";
import { subscribeToBrowserEvent } from "../event-subscription.ts";
/**
 * Subscribes to focus events on the supplied browser window, without an initial
 * notification. Removal cancels pending debounced delivery. The listener and
 * its error handler must be synchronous. If the window's `removeEventListener`
 * throws, the returned remover finishes its cleanup and then throws an
 * AggregateError.
 */
export function subscribeToWindowFocus(input: {
	readonly window: Window;
	readonly timing:
		| { readonly kind: "immediate" }
		| { readonly kind: "debounced"; readonly delayMillis: number };
	readonly listener: () => void;
	readonly onListenerErrorFn: (cause: unknown) => void;
}): Result<() => void, DebounceConfigError> {
	return subscribeToBrowserEvent({
		...input,
		target: input.window,
		eventName: "focus",
		readEventValueFn: () => undefined,
	});
}
