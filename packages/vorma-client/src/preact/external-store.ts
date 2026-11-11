import { useLayoutEffect, useRef, useState } from "preact/hooks";
import { callWithResult } from "primitives-ts/results";

/**
 * Subscribes before the first paint so that a Vorma commit synchronously updates
 * the DOM before the core releases the previous frame's loader resources.
 * Preact's compatibility hook subscribes in a passive effect.
 */
export function useSyncExternalStore<Value>(
	subscribe: (listener: () => void) => () => void,
	getSnapshot: () => Value,
): Value {
	const value = getSnapshot();
	const latest = useRef({ value, getSnapshot });
	const [, setRevision] = useState(0);
	useLayoutEffect(() => {
		latest.current = { value, getSnapshot };
	});
	useLayoutEffect(() => {
		function checkSnapshot(): void {
			const next = callWithResult(latest.current.getSnapshot);
			if (
				next.kind === "error" ||
				!Object.is(latest.current.value, next.value)
			) {
				setRevision((revision) => revision + 1);
			}
		}
		const stop = subscribe(checkSnapshot);
		checkSnapshot();
		return stop;
	}, [subscribe]);
	return value;
}
