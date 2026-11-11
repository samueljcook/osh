import type { ComponentType, ReactElement } from "react";
import { flushSync } from "react-dom";
import { createClientCore } from "../adapter-api.ts";
import type {
	CoreOptions,
	ViewDeclaredErrorOf,
	PageError,
	ViewPattern,
	VormaClient,
} from "../index.ts";
import { AdapterProvider, type ErrorComponent } from "./context.ts";
import type { DefaultLinkProps } from "./link.tsx";
import { ViewOutlet } from "./outlet.tsx";

export type VormaClientOptions = CoreOptions & {
	/**
	 * Vorma calls it once, after the first page commits. It mounts `RootOutlet`
	 * with the renderer and returns a function that unmounts it, which Vorma
	 * calls when the client is disposed.
	 */
	readonly renderFn: (args: {
		readonly RootOutlet: ComponentType;
		readonly rootElement: HTMLElement;
	}) => () => void;
	/**
	 * Renders a failure at a position when the position's parent view has no
	 * `childErrorComponent`, and renders a failed boot.
	 */
	readonly defaultErrorComponent: ComponentType<{
		readonly error: PageError<ViewDeclaredErrorOf<ViewPattern>>;
	}>;
	/** Props that every `Link` starts from; a prop on the link wins. */
	readonly defaultLinkProps?: DefaultLinkProps;
};

/**
 * Applications must declare `react` and `react-dom`, 18.0.0 or later, which
 * provide `useSyncExternalStore` and `flushSync`. The adapter's declarations
 * need `@types/react`; an application that calls React DOM's `createRoot` in
 * TypeScript also needs `@types/react-dom` for its own code.
 *
 * Creates the document's Vorma client. Creating it starts nothing and touches
 * no document; `boot()` starts it. A document has one client: while a client
 * that has not been disposed exists, a client that this function creates is
 * not the document's client, its `boot()` fails, and the Vorma functions keep
 * acting on the first client.
 */
export function createVormaClient(options: VormaClientOptions): VormaClient {
	const {
		renderFn,
		defaultErrorComponent,
		defaultLinkProps,
		...coreOptions
	} = options;
	const ctx = {
		defaultErrorComponent: defaultErrorComponent as ErrorComponent,
		defaultLinkProps,
	};
	function RootOutlet(): ReactElement {
		return (
			<AdapterProvider value={ctx}>
				<ViewOutlet positionIndex={0} />
			</AdapterProvider>
		);
	}
	return createClientCore({
		options: coreOptions,
		renderer: {
			mountRoot: (rootElement) => renderFn({ RootOutlet, rootElement }),
			flushSync: (apply) => {
				flushSync(apply);
			},
		},
	});
}
