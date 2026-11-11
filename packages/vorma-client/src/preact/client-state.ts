import { useRef } from "preact/hooks";
import { createClientStateHook } from "../adapter-shared/client-state.ts";
import { useSyncExternalStore } from "./external-store.ts";
/**
 * Selects committed client state and subscribes for the component's lifetime.
 * Equal selections retain their identity. Comparison inspects own enumerable
 * string and symbol properties of arrays and plain objects through three
 * levels; deeper values and other objects use identity. Selectors must be pure.
 * The hook requires a committed page and throws before the first commit.
 */
export const useClientState = createClientStateHook({
	useRef,
	useSyncExternalStore,
});
