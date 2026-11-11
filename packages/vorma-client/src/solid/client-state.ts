import {
	createMemo,
	createSignal,
	onCleanup,
	untrack,
	type Accessor,
} from "solid-js";
import { compareSelection } from "../adapter-shared/client-state.ts";
import {
	getClientState,
	subscribeToClientState,
	type ClientState,
} from "../index.ts";

/**
 * Returns an accessor of a selection of committed client state and releases
 * its subscription with the Solid owner. Call the accessor inside JSX or a
 * reactive computation to read the selected value:
 *
 * ```tsx
 * const href = useClientState((state) => state.page.href);
 * return <p>{href()}</p>;
 * ```
 *
 * Comparison inspects own enumerable string and symbol properties of arrays
 * and plain objects through three levels; deeper values and other objects use
 * identity. Selectors must be pure. Reading the accessor before the first page
 * commits throws.
 */
export function useClientState<Selected>(
	selectFn: (state: ClientState) => Selected,
): Accessor<Selected> {
	const [state, setState] = createSignal(untrack(getClientState), {
		ownedWrite: true,
	});
	onCleanup(subscribeToClientState((next) => setState(next)));
	return createMemo(
		() => {
			const current = state();
			if (current === undefined) {
				throw new Error(
					"useClientState needs the first page to have committed.",
				);
			}
			return selectFn(current);
		},
		{
			equals: (previous, next) =>
				compareSelection({ previous, next, remainingDepth: 3 }),
		},
	);
}
