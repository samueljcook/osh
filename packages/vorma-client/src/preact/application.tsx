/** @jsxImportSource preact */

import { options, type ComponentType, type JSX } from "preact";
import { callWithResult } from "primitives-ts/results";
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
 * Runs `apply` and renders the components that it marked for rendering before
 * it returns. Preact schedules a render through the public
 * `options.debounceRendering` hook, so the binding captures the scheduled
 * render while `apply` runs and calls it at once; the application's own hook
 * applies again afterward. A failure of `apply` reaches the caller as a throw,
 * as the engine's renderer binding expects from a renderer's flush.
 */
function flushPreactRender(apply: () => void): void {
	type ScheduledRender =
		| { readonly kind: "none" }
		| { readonly kind: "scheduled"; readonly render: () => void };
	const capture: { scheduled: ScheduledRender } = {
		scheduled: { kind: "none" },
	};
	// Preact declares the hook as a method; the binding keeps the application's
	// own function only to put it back unchanged.
	// oxlint-disable-next-line typescript/unbound-method
	const applicationDebounce = options.debounceRendering;
	options.debounceRendering = (render) => {
		capture.scheduled = { kind: "scheduled", render };
	};
	const applied = callWithResult(apply);
	if (applicationDebounce === undefined) {
		delete options.debounceRendering;
	} else {
		options.debounceRendering = applicationDebounce;
	}
	const { scheduled } = capture;
	if (scheduled.kind === "scheduled") {
		scheduled.render();
	}
	if (applied.kind === "error") {
		throw applied.error;
	}
}

/**
 * Applications must declare Preact 11 and compile JSX with Preact as its
 * import source. The adapter uses only Preact itself and `preact/hooks`, so
 * it leaves `preact/compat` and its React compatibility behavior out of the
 * application. Use `useLayoutEffect` when cleanup must finish before Vorma
 * releases a departing view's loader resources.
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
				<ViewOutlet positionIndex={0} />
			</AdapterProvider>
		);
	}
	return createClientCore({
		options: coreOptions,
		renderer: {
			mountRoot: (rootElement) => renderFn({ RootOutlet, rootElement }),
			flushSync: flushPreactRender,
		},
	});
}
