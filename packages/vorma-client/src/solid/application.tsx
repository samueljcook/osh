/** @jsxImportSource @solidjs/web */

import type { JSX } from "@solidjs/web";
import { flush, type Component } from "solid-js";
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
import { RootViewOutlet } from "./outlet.tsx";

export type VormaClientOptions = CoreOptions & {
	/**
	 * Vorma calls it once, after the first page commits. It mounts `RootOutlet`
	 * with the renderer and returns a function that unmounts it, which Vorma
	 * calls when the client is disposed.
	 */
	readonly renderFn: (args: {
		readonly RootOutlet: Component;
		readonly rootElement: HTMLElement;
	}) => () => void;
	/**
	 * Renders a failure at a position when the position's parent view has no
	 * `childErrorComponent`, and renders a failed boot.
	 */
	readonly defaultErrorComponent: Component<{
		readonly error: PageError<ViewDeclaredErrorOf<ViewPattern>>;
	}>;
	/** Props that every `Link` starts from; a prop on the link wins. */
	readonly defaultLinkProps?: DefaultLinkProps;
};

/**
 * Applications must declare matching Solid 2 versions of `solid-js` and
 * `@solidjs/web` at `2.0.0-rc.18` or later because Vorma compiles the
 * published adapter with `@solidjs/compiler`, which imports its runtime
 * helpers from `@solidjs/web`. Components
 * receive reactive props: read `props.serverOutput`, `props.clientOutput`, and
 * the other props inside JSX or reactive computations because destructuring
 * them when the component is created captures their initial values. Matched
 * parent views keep their component state across a navigation that changes
 * only a child.
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
	function RootOutlet(): JSX.Element {
		return (
			<AdapterProvider value={ctx}>
				<RootViewOutlet />
			</AdapterProvider>
		);
	}
	return createClientCore({
		options: coreOptions,
		renderer: {
			mountRoot: (rootElement) => renderFn({ RootOutlet, rootElement }),
			flushSync: (apply) => {
				flush(apply);
			},
		},
	});
}
