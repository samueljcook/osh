import {
	getClientState,
	subscribeToClientState,
	type ClientState,
} from "../index.ts";
import type { HookRuntime } from "./hooks.ts";

/*
Components read the client state through `useClientState`. The snapshot keeps
the identity of every part that did not change, so a selection that returns an
unchanged part is the same value as before. A selection that builds a new value
from unchanged parts, such as the result of `getLinkState`, keeps its earlier
identity too, because the comparison below finds it equal.
*/

/** How many levels of nested plain objects and arrays a selection comparison inspects. */
const COMPARED_DEPTH = 3;

function checkPlainObject(value: object): value is Record<string, unknown> {
	const prototype: unknown = Object.getPrototypeOf(value);
	return prototype === Object.prototype || prototype === null;
}

/** Whether two selections are the same value, or plain objects or arrays with matching contents. */
export function compareSelection(input: {
	readonly previous: unknown;
	readonly next: unknown;
	readonly remainingDepth: number;
}): boolean {
	const { previous, next, remainingDepth } = input;
	if (Object.is(previous, next)) {
		return true;
	}
	if (
		remainingDepth === 0 ||
		typeof previous !== "object" ||
		typeof next !== "object" ||
		previous === null ||
		next === null
	) {
		return false;
	}
	if (Object.getPrototypeOf(previous) !== Object.getPrototypeOf(next)) {
		return false;
	}
	if (Array.isArray(previous)) {
		if (!Array.isArray(next) || previous.length !== next.length) {
			return false;
		}
	} else if (
		Array.isArray(next) ||
		!checkPlainObject(previous) ||
		!checkPlainObject(next)
	) {
		return false;
	}
	const keys = getEnumerableKeys(previous);
	return (
		keys.length === getEnumerableKeys(next).length &&
		keys.every((key) => {
			return (
				Object.prototype.propertyIsEnumerable.call(next, key) &&
				compareSelection({
					previous: Reflect.get(previous, key),
					next: Reflect.get(next, key),
					remainingDepth: remainingDepth - 1,
				})
			);
		})
	);
}

function getEnumerableKeys(value: object): Array<PropertyKey> {
	return Reflect.ownKeys(value).filter((key) =>
		Object.prototype.propertyIsEnumerable.call(value, key),
	);
}

type Selection<Selected> = {
	readonly state: ClientState;
	readonly selectFn: (state: ClientState) => Selected;
	readonly selected: Selected;
};

function subscribeToState(onChange: () => void): () => void {
	return subscribeToClientState(() => onChange());
}

export function createClientStateHook({
	useRef,
	useSyncExternalStore,
}: Pick<HookRuntime, "useRef" | "useSyncExternalStore">) {
	/**
	 * The selected part of the client state. The component renders again only when
	 * the selected value changes.
	 *
	 * It throws in a component that renders before the first page has committed,
	 * which only a component outside the tree that Vorma mounts can do.
	 */
	function useClientState<Selected>(
		selectFn: (state: ClientState) => Selected,
	): Selected {
		const selection = useRef<Selection<Selected> | undefined>(undefined);
		function getSnapshot(): Selected {
			const state = getClientState();
			if (state === undefined) {
				throw new Error(
					"useClientState needs the first page to have committed. Use it in a component that Vorma renders, or after boot has finished.",
				);
			}
			const previous = selection.current;
			if (
				previous !== undefined &&
				previous.state === state &&
				previous.selectFn === selectFn
			) {
				return previous.selected;
			}
			const next = selectFn(state);
			const selected =
				previous !== undefined &&
				compareSelection({
					previous: previous.selected,
					next,
					remainingDepth: COMPARED_DEPTH,
				})
					? previous.selected
					: next;
			selection.current = { state, selectFn, selected };
			return selected;
		}
		return useSyncExternalStore(subscribeToState, getSnapshot);
	}

	return useClientState;
}
