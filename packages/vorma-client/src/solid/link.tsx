/** @jsxImportSource @solidjs/web */

import type { JSX } from "@solidjs/web";
import {
	children,
	createEffect,
	createMemo,
	omit,
	onCleanup,
	Show,
} from "solid-js";
import { reportLinkFailure } from "../adapter-api.ts";
import { pickDefinedFields } from "../adapter-shared/defined-fields.ts";
import {
	computeLinkStateAttrs,
	createLinkInteraction,
	selectLinkSelection,
	type LinkBehaviorProps,
} from "../adapter-shared/link.ts";
import { toHref, type NavTarget } from "../index.ts";
import { useClientState } from "./client-state.ts";
import { useAdapterCtx } from "./context.ts";

type AnchorProps = Omit<JSX.AnchorHTMLAttributes<HTMLAnchorElement>, "href">;
export type DefaultLinkProps = Partial<
	LinkBehaviorProps & Omit<AnchorProps, "children">
>;
export type LinkProps = NavTarget & LinkBehaviorProps & AnchorProps;
type RuntimeLinkProps = DefaultLinkProps & {
	readonly href?: string;
	readonly pattern?: string;
	readonly params?: Readonly<Record<string, string>>;
	readonly splatValues?: ReadonlyArray<string>;
	readonly input?: unknown;
	readonly hash?: string;
};

type AnchorEvent<E extends Event> = E & {
	currentTarget: HTMLAnchorElement;
	target: Element;
};

function callHandler<E extends Event>(
	handler: JSX.EventHandlerUnion<HTMLAnchorElement, E> | undefined,
	event: AnchorEvent<E>,
): void {
	if (typeof handler === "function") {
		handler(event);
	} else if (handler !== undefined) {
		handler[0](handler[1], event);
	}
}

/**
 * An anchor with reactive active and pending state, typed navigation, and
 * optional intent prefetch. A click that the browser would handle in the same
 * browsing context navigates through `navigate`, with the link's navigation
 * options. Focus follows the scroll that the navigation performs, as
 * `navigate` describes: a scroll to the top or to a restored position resets
 * focus to the document, a scroll to a fragment target focuses the target or
 * makes it the starting point of sequential focus navigation, and a
 * navigation without a scroll, for example with `scrollToTop={false}` and no
 * fragment target, keeps focus where it is. The link resolves its children once, so a change of
 * another prop keeps them.
 */
export function Link(props: LinkProps): JSX.Element {
	const { defaultLinkProps } = useAdapterCtx();
	const resolvedChildren = children(() => props.children);
	const linkProps = omit(props, "children");
	const values = createMemo(() => {
		const {
			href,
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
			...anchorProps
		} = { ...defaultLinkProps, ...linkProps } as RuntimeLinkProps;
		const resolved = toHref({
			href,
			pattern,
			params,
			splatValues,
			input,
			hash,
		} as NavTarget);
		return {
			resolved,
			pattern,
			linkStateRules,
			behavior: {
				href: resolved.kind === "href" ? resolved.href : undefined,
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
			},
			anchorProps,
		};
	});
	createEffect(
		() => values().resolved,
		(result) => {
			if (result.kind === "invalid-target") {
				reportLinkFailure(new Error(result.msg));
			}
		},
	);
	const selection = useClientState(selectLinkSelection);
	const interaction = createLinkInteraction({
		readBehaviorFn: () => values().behavior,
	});
	onCleanup(() => interaction.reset());
	const stateAttrs = createMemo(() => {
		const current = values();
		return computeLinkStateAttrs({
			href: current.behavior.href,
			targetPattern: current.pattern,
			selection: selection(),
			rules: current.linkStateRules,
			ariaCurrent: current.anchorProps["aria-current"],
		});
	});
	return (
		<Show
			when={!selection().bootFailed}
			fallback={
				<a {...values().anchorProps} href={values().behavior.href}>
					{resolvedChildren()}
				</a>
			}
		>
			<a
				{...values().anchorProps}
				{...stateAttrs()}
				href={values().behavior.href}
				onClick={(event) => {
					interaction.handleClick({
						event,
						callApplicationHandlerFn: () => {
							callHandler(values().anchorProps.onClick, event);
						},
					});
				}}
				onPointerDown={(event) => {
					interaction.handlePointerDown({
						event,
						callApplicationHandlerFn: () => {
							callHandler(
								values().anchorProps.onPointerDown,
								event,
							);
						},
					});
				}}
				onKeyDown={(event) => {
					interaction.endPointerActivation();
					callHandler(values().anchorProps.onKeyDown, event);
				}}
				onDragStart={(event) => {
					interaction.endPointerActivation();
					callHandler(values().anchorProps.onDragStart, event);
				}}
				onPointerEnter={(event) => {
					interaction.startIntent("pointer");
					callHandler(values().anchorProps.onPointerEnter, event);
				}}
				onPointerLeave={(event) => {
					interaction.endIntent("pointer");
					callHandler(values().anchorProps.onPointerLeave, event);
				}}
				onFocus={(event) => {
					interaction.startIntent("focus");
					callHandler(values().anchorProps.onFocus, event);
				}}
				onBlur={(event) => {
					interaction.endIntent("focus");
					callHandler(values().anchorProps.onBlur, event);
				}}
				onPointerCancel={(event) => {
					interaction.cancelPointer();
					callHandler(values().anchorProps.onPointerCancel, event);
				}}
				onTouchCancel={(event) => {
					interaction.cancelPointer();
					callHandler(values().anchorProps.onTouchCancel, event);
				}}
			>
				{resolvedChildren()}
			</a>
		</Show>
	);
}
