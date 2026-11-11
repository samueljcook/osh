import { createContext, useContext, type Component } from "solid-js";
import type { PageError } from "../index.ts";
import type { DefaultLinkProps } from "./link.tsx";

export type ErrorComponent = Component<{ readonly error: PageError }>;
export type AdapterCtx = {
	readonly defaultErrorComponent: ErrorComponent;
	readonly defaultLinkProps: DefaultLinkProps | undefined;
};
export const AdapterProvider = createContext<AdapterCtx>();
export function useAdapterCtx(): AdapterCtx {
	return useContext(AdapterProvider);
}
