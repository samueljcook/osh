import { callWithResult } from "../results/index.ts";

/** A published change retains the values supplied to the observable value. */
export type ObservableValueChange<Value> = {
	readonly value: Value;
	readonly previousValue: Value;
};

/** A value with synchronous writes and ordered change notifications. */
export type ObservableValue<Value> = {
	readonly getValue: () => Value;
	readonly setValue: (value: Value) => void;
	readonly updateValue: (updateFn: (value: Value) => Value) => void;
	/**
	 * Subscribes to future changes without an initial notification and returns an
	 * idempotent remover. Each registration is independent. Listeners added after
	 * a write do not receive that write; removal prevents pending delivery.
	 */
	readonly subscribe: (
		listener: (change: ObservableValueChange<Value>) => void,
	) => () => void;
};

/**
 * Creates an observable value without notifying listeners. Equal writes retain the
 * existing value. Reentrant writes update the value immediately and queue their
 * notifications after the change being delivered. Listeners and error reporting
 * are synchronous; comparator and update-function exceptions propagate.
 * Values retain their identity without cloning or freezing. A reentrant write
 * can make `getValue()` newer than the notification that a listener receives.
 * Listener failures go to `onListenerErrorFn` while other listeners continue.
 * If error reporting throws, queued notifications finish before an
 * `AggregateError` reports those failures. Comparators must be pure.
 */
export function createObservableValue<Value>(input: {
	readonly initialValue: Value;
	readonly compareValuesForEqualityFn: (values: {
		readonly left: Value;
		readonly right: Value;
	}) => boolean;
	readonly onListenerErrorFn: (cause: unknown) => void;
}): ObservableValue<Value> {
	type Registration = {
		state:
			| {
					readonly kind: "subscribed";
					readonly listener: (
						change: ObservableValueChange<Value>,
					) => void;
			  }
			| { readonly kind: "removed" };
	};
	type Notification = {
		readonly change: ObservableValueChange<Value>;
		readonly listeners: ReadonlyArray<Registration>;
	};
	type DeliveryState =
		| { readonly kind: "idle" }
		| {
				readonly kind: "notifying";
				readonly notifications: Array<Notification>;
		  };
	let current = input.initialValue;
	const listeners = new Set<Registration>();
	let delivery: DeliveryState = { kind: "idle" };

	function setValue(value: Value): void {
		if (input.compareValuesForEqualityFn({ left: current, right: value })) {
			return;
		}
		const change = { value, previousValue: current };
		current = value;
		const notification = { change, listeners: [...listeners] };
		if (delivery.kind === "notifying") {
			delivery.notifications.push(notification);
			return;
		}
		const notifying: Extract<DeliveryState, { kind: "notifying" }> = {
			kind: "notifying",
			notifications: [notification],
		};
		delivery = notifying;
		const reportingErrors: Array<unknown> = [];
		for (
			let index = 0;
			index < notifying.notifications.length;
			index += 1
		) {
			const notification = notifying.notifications[index]!;
			for (const registration of notification.listeners) {
				if (registration.state.kind === "removed") {
					continue;
				}
				const { listener } = registration.state;
				const result = callWithResult(() =>
					listener(notification.change),
				);
				if (result.kind === "error") {
					const reported = callWithResult(() =>
						input.onListenerErrorFn(result.error),
					);
					if (reported.kind === "error") {
						reportingErrors.push(reported.error);
					}
				}
			}
		}
		notifying.notifications.length = 0;
		delivery = { kind: "idle" };
		if (reportingErrors.length > 0) {
			throw new AggregateError(
				reportingErrors,
				"The observable-value listener error handler failed.",
			);
		}
	}

	return {
		getValue: () => current,
		setValue,
		updateValue: (updateFn) => setValue(updateFn(current)),
		subscribe: (listener) => {
			const registration: Registration = {
				state: { kind: "subscribed", listener },
			};
			listeners.add(registration);
			return () => {
				registration.state = { kind: "removed" };
				listeners.delete(registration);
			};
		},
	};
}
