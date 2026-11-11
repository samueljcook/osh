import type { Frame } from "../adapter-api.ts";

/*
The renderer-neutral part of every adapter's outlet chain: which error
component renders a failure at a position, the depth at which the destination
exists in the DOM, and the error that hands a render failure from a boundary
without presentation to the boundary above it.

A failure renders at its own position: the position's error component is its
parent's `childErrorComponent`, or the application's `defaultErrorComponent`
when the parent defines none. Selection never looks further up the tree, so
everything that succeeded above the failure stays visible, and every view can
have error components specific to it. An error component that itself throws
falls back to the default component at the same position; only a throwing
default component reaches the position above.
*/

/** A render failure with the position that it belongs to. */
export type RenderFailure = {
	readonly cause: unknown;
	readonly positionIndex: number;
	/** The pattern of the failing position; undefined when the position has no matched view. */
	readonly pattern: string | undefined;
};

/**
 * A position's error components: the component that renders a failure at the
 * position, and the default component that renders a failure of that
 * component, which is undefined when the position's component is the default
 * component itself.
 */
export function selectErrorComponents<ErrorComponent>(input: {
	readonly frame: Frame;
	readonly defaultErrorComponent: ErrorComponent;
	readonly positionIndex: number;
}): {
	readonly positionErrorComponent: ErrorComponent;
	readonly fallbackErrorComponent: ErrorComponent | undefined;
} {
	const parent = input.frame.entries[input.positionIndex - 1];
	const childErrorComponent = parent?.view?.childErrorComponent as
		| ErrorComponent
		| undefined;
	return childErrorComponent === undefined ||
		childErrorComponent === input.defaultErrorComponent
		? {
				positionErrorComponent: input.defaultErrorComponent,
				fallbackErrorComponent: undefined,
			}
		: {
				positionErrorComponent: childErrorComponent,
				fallbackErrorComponent: input.defaultErrorComponent,
			};
}

/**
 * The depth at which the destination exists in the DOM once it has rendered:
 * the failing position, or the last entry.
 */
export function getDestinationDepth(frame: Frame): number {
	return Math.max(0, frame.error?.positionIndex ?? frame.entries.length - 1);
}

const ATTRIBUTED_FAILURE: unique symbol = Symbol(
	"@vorma/client/AttributedFailure",
);

/**
 * Creates the error that a boundary without presentation throws to hand a
 * render failure to the boundary above it. It is an ordinary `Error` with the
 * failure's own cause as its `cause`, so that error reporting receives a stack
 * and the original failure.
 */
export function createAttributedFailure(failure: RenderFailure): Error {
	const error = new Error(
		`A view failed to render at position ${failure.positionIndex}.`,
		{ cause: failure.cause },
	);
	Object.defineProperty(error, ATTRIBUTED_FAILURE, { value: failure });
	return error;
}

/**
 * The render failure that a caught value carries: the failure of an attributed
 * failure, or a new failure at this position for any other value.
 */
export function attributeRenderFailure(input: {
	readonly caught: unknown;
	readonly positionIndex: number;
	readonly pattern: string | undefined;
}): RenderFailure {
	const { caught } = input;
	if (
		typeof caught === "object" &&
		caught !== null &&
		Object.hasOwn(caught, ATTRIBUTED_FAILURE)
	) {
		return (caught as { readonly [ATTRIBUTED_FAILURE]: RenderFailure })[
			ATTRIBUTED_FAILURE
		];
	}
	return {
		cause: caught,
		positionIndex: input.positionIndex,
		pattern: input.pattern,
	};
}
