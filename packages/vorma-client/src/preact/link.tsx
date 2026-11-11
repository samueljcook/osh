/** @jsxImportSource preact */

import type { ComponentChildren, ComponentProps, JSX, VNode } from "preact";
import { useEffect, useLayoutEffect, useRef, useState } from "preact/hooks";
import { reportLinkFailure } from "../adapter-api.ts";
import { pickDefinedFields } from "../adapter-shared/defined-fields.ts";
import {
	computeLinkStateAttrs,
	createLinkInteraction,
	selectLinkSelection,
	type LinkBehavior,
	type LinkBehaviorProps,
} from "../adapter-shared/link.ts";
import { toHref, type NavTarget } from "../index.ts";
import { useClientState } from "./client-state.ts";
import { useAdapterCtx } from "./context.ts";

/*
Links project active and pending state. Intent prefetch is opt-in and holds a
prefetch while a pointer or focus rests on the link. Modified clicks, downloads,
other targets, and external links keep the browser's own handling. A link with
a target that cannot be encoded renders an anchor without an `href`. A link on
the boot failure screen is a plain anchor, because no client handles its
activation.
*/

export type DefaultLinkProps = Partial<
	LinkBehaviorProps & Omit<ComponentProps<"a">, "href" | "children">
>;

type AnchorProps = LinkBehaviorProps &
	Omit<ComponentProps<"a">, "href"> & {
		readonly children?: ComponentChildren;
	};

export type LinkProps = NavTarget & AnchorProps;

/** `LinkProps` with the target fields of every variant, as the implementation reads them. */
type RuntimeLinkProps = AnchorProps & {
	readonly href?: string | undefined;
	readonly pattern?: string | undefined;
	readonly params?: Readonly<Record<string, string>> | undefined;
	readonly splatValues?: ReadonlyArray<string> | undefined;
	readonly input?: unknown;
	readonly hash?: string | undefined;
};

/**
 * An anchor that navigates within the application; the generated types check
 * its target. A click that the browser would handle in the same browsing
 * context navigates through `navigate`, with the link's navigation options.
 * Focus follows the scroll that the navigation performs, as `navigate`
 * describes: a scroll to the top or to a restored position resets focus to
 * the document, a scroll to a fragment target focuses the target or makes it
 * the starting point of sequential focus navigation, and a navigation without
 * a scroll, for example with `scrollToTop={false}` and no fragment target,
 * keeps focus where it is.
 */
export function Link(props: LinkProps): VNode {
	const { defaultLinkProps } = useAdapterCtx();
	const {
		href: targetHref,
		pattern,
		params,
		splatValues,
		input,
		hash,
		prefetch,
		prefetchDelayMillis,
		navigateOnPointerDown,
		linkStateRules,
		replace,
		scrollToTop,
		scrollToHash,
		historyState,
		skipWorkIndicator,
		children,
		...anchorProps
	} = { ...defaultLinkProps, ...props } as RuntimeLinkProps;
	const resolved = toHref({
		href: targetHref,
		pattern,
		params,
		splatValues,
		input,
		hash,
	} as NavTarget);
	const href = resolved.kind === "href" ? resolved.href : undefined;
	const invalidMsg =
		resolved.kind === "invalid-target" ? resolved.msg : undefined;
	useEffect(() => {
		if (invalidMsg !== undefined) {
			reportLinkFailure(new Error(invalidMsg));
		}
	}, [invalidMsg]);
	const selection = useClientState(selectLinkSelection);
	const behavior: LinkBehavior = {
		href,
		prefetch,
		prefetchDelayMillis,
		navigateOnPointerDown,
		navOptions: pickDefinedFields({
			replace,
			scrollToTop,
			scrollToHash,
			historyState,
			skipWorkIndicator,
		}),
	};
	const latestBehavior = useRef(behavior);
	useLayoutEffect(() => {
		latestBehavior.current = behavior;
	});
	const [interaction] = useState(() => {
		return createLinkInteraction({
			readBehaviorFn: () => latestBehavior.current,
		});
	});
	useEffect(() => () => interaction.reset(), [interaction]);
	const linkAttrs = {
		...anchorProps,
		...pickDefinedFields({ href }),
	} as JSX.IntrinsicElements["a"];
	if (selection.bootFailed) {
		return <a {...linkAttrs}>{children}</a>;
	}
	const stateAttrs = computeLinkStateAttrs({
		href,
		targetPattern: pattern,
		selection,
		rules: linkStateRules,
		ariaCurrent: anchorProps["aria-current"],
	});
	return (
		<a
			{...linkAttrs}
			{...stateAttrs}
			onClick={(event) => {
				interaction.handleClick({
					event,
					callApplicationHandlerFn: () =>
						anchorProps.onClick?.(event),
				});
			}}
			onPointerDown={(event) => {
				interaction.handlePointerDown({
					event,
					callApplicationHandlerFn: () =>
						anchorProps.onPointerDown?.(event),
				});
			}}
			onKeyDown={(event) => {
				interaction.endPointerActivation();
				anchorProps.onKeyDown?.(event);
			}}
			onDragStart={(event) => {
				interaction.endPointerActivation();
				anchorProps.onDragStart?.(event);
			}}
			onPointerEnter={(event) => {
				interaction.startIntent("pointer");
				anchorProps.onPointerEnter?.(event);
			}}
			onPointerLeave={(event) => {
				interaction.endIntent("pointer");
				anchorProps.onPointerLeave?.(event);
			}}
			onFocus={(event) => {
				interaction.startIntent("focus");
				anchorProps.onFocus?.(event);
			}}
			onBlur={(event) => {
				interaction.endIntent("focus");
				anchorProps.onBlur?.(event);
			}}
			onPointerCancel={(event) => {
				interaction.cancelPointer();
				anchorProps.onPointerCancel?.(event);
			}}
			onTouchCancel={(event) => {
				interaction.cancelPointer();
				anchorProps.onTouchCancel?.(event);
			}}
		>
			{children}
		</a>
	);
}
