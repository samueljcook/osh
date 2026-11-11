import { createContext, useContext, type ComponentType } from "react";
import type { PageError } from "../index.ts";
import type { DefaultLinkProps } from "./link.tsx";

/** An error component as the outlet renders it, with the failure that it received. */
export type ErrorComponent = ComponentType<{ readonly error: PageError }>;

/** The options of `createVormaClient` that components read while they render. */
export type AdapterCtx = {
	readonly defaultErrorComponent: ErrorComponent;
	readonly defaultLinkProps: DefaultLinkProps | undefined;
};

const ReactAdapterCtx = createContext<AdapterCtx | null>(null);

export const AdapterProvider = ReactAdapterCtx.Provider;

/** Reads the adapter's options. A component outside Vorma's tree cannot work, and React's protocol for that is a throw. */
export function useAdapterCtx(): AdapterCtx {
	const ctx = useContext(ReactAdapterCtx);
	if (ctx === null) {
		throw new Error(
			"A Vorma React component renders only inside the tree that createVormaClient mounts.",
		);
	}
	return ctx;
}
