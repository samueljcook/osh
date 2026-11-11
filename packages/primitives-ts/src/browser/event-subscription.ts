import {
	createDebouncedFunction,
	type DebounceConfigError,
	type DebouncedCallError,
	type DebouncedFunction,
} from "../async-work/debounce/index.ts";
import { createResourceScope } from "../resource-scopes/index.ts";
import {
	callWithResult,
	createSuccessResult,
	type Result,
} from "../results/index.ts";

/**
 * Creates independently removable delivery for one native event source. The
 * returned remover is idempotent. If the target's `removeEventListener`
 * throws, the remover finishes its cleanup and then throws an AggregateError.
 */
export function subscribeToBrowserEvent<Value>(input: {
	readonly target: EventTarget;
	readonly eventName: string;
	readonly readEventValueFn: () => Value;
	readonly timing:
		| { readonly kind: "immediate" }
		| { readonly kind: "debounced"; readonly delayMillis: number };
	readonly listener: (value: Value) => void;
	readonly onListenerErrorFn: (cause: unknown) => void;
}): Result<() => void, DebounceConfigError> {
	const debounced =
		input.timing.kind === "debounced"
			? createDebouncedFunction({
					delayMillis: input.timing.delayMillis,
					operationFn: (event: Value) => event,
				})
			: undefined;
	if (debounced?.kind === "error") {
		return debounced;
	}

	type DebouncedDelivery = DebouncedFunction<[Value], Awaited<Value>>;
	/** The scope's signal is the only record of removal. */
	type Delivery =
		| { readonly kind: "immediate" }
		| {
				readonly kind: "debounced";
				readonly debounce: DebouncedDelivery;
				/** The most recent batch promise; its delivery continuation is already attached. */
				lastBatchPromise: Promise<
					Result<Value, DebouncedCallError>
				> | null;
		  };
	const delivery: Delivery =
		debounced === undefined
			? { kind: "immediate" }
			: {
					kind: "debounced",
					debounce: debounced.value,
					lastBatchPromise: null,
				};
	const scope = createResourceScope({
		onCleanupErrorFn: (cause) => {
			throw cause;
		},
	});
	scope.addCleanup(() => {
		if (delivery.kind === "debounced") {
			delivery.debounce.cancelPendingCalls(undefined);
		}
	});

	function notify(event: Value): void {
		const result = callWithResult(() => input.listener(event));
		if (result.kind === "error") {
			input.onListenerErrorFn(result.error);
		}
	}

	function handleEvent(event: Value): void {
		if (scope.signal.aborted) {
			return;
		}
		if (delivery.kind === "immediate") {
			notify(event);
			return;
		}
		const promise = delivery.debounce(event);
		if (promise === delivery.lastBatchPromise) {
			return;
		}
		delivery.lastBatchPromise = promise;
		void promise.then((result) => {
			if (!scope.signal.aborted && result.kind === "ok") {
				notify(result.value);
			}
		});
	}

	function handleNativeEvent(): void {
		if (!scope.signal.aborted) {
			handleEvent(input.readEventValueFn());
		}
	}
	input.target.addEventListener(input.eventName, handleNativeEvent);
	scope.addCleanup(() => {
		input.target.removeEventListener(input.eventName, handleNativeEvent);
	});
	return createSuccessResult(scope.dispose);
}
