/** @jsxImportSource @solidjs/web */

import { dynamic, type JSX } from "@solidjs/web";
import {
	createContext,
	createEffect,
	createMemo,
	createSignal,
	Errored,
	onCleanup,
	onSettled,
	Show,
	untrack,
	useContext,
	type Accessor,
	type Component,
} from "solid-js";
import {
	consumeScrollIntent,
	getRenderFrame,
	reportRenderFailure,
	subscribeToRenderFrame,
	toBrowserPageError,
	type Frame,
} from "../adapter-api.ts";
import {
	attributeRenderFailure,
	createAttributedFailure,
	getDestinationDepth,
	selectErrorComponents,
} from "../adapter-shared/outlet.ts";
import { useAdapterCtx, type ErrorComponent } from "./context.ts";

const FrameProvider = createContext<Accessor<Frame | undefined>>();

/** Bridges each core commit into one reactive frame for the entire outlet tree. */
export function RootViewOutlet(): JSX.Element {
	const [frame, setFrame] = createSignal(untrack(getRenderFrame), {
		ownedWrite: true,
	});
	onCleanup(subscribeToRenderFrame((next) => setFrame(next)));
	return (
		<FrameProvider value={frame}>
			<ViewOutlet positionIndex={0} />
		</FrameProvider>
	);
}

type BoundaryProps = {
	readonly positionIndex: number;
	readonly errorComponent: ErrorComponent | undefined;
	readonly fallbackErrorComponent: ErrorComponent | undefined;
	readonly children: JSX.Element;
};

function ViewBoundary(props: BoundaryProps): JSX.Element {
	const frame = useContext(FrameProvider);
	return (
		<Errored
			fallback={(cause, reset) => {
				const failedRevision = untrack(() => frame()?.revision);
				const failure = createMemo(() => {
					return attributeRenderFailure({
						caught: cause(),
						positionIndex: props.positionIndex,
						pattern: frame()?.entries[props.positionIndex]?.pattern,
					});
				});
				createEffect(
					() => frame()?.revision,
					(revision) => {
						if (revision !== failedRevision) {
							reset();
						}
					},
				);
				onSettled(() => {
					if (
						props.errorComponent === undefined ||
						failedRevision === undefined
					) {
						return;
					}
					reportRenderFailure({
						revision: failedRevision,
						...failure(),
					});
				});
				const Error = dynamic(() => props.errorComponent);
				return (
					<Show
						when={props.errorComponent}
						fallback={(() => {
							throw createAttributedFailure(failure());
						})()}
					>
						<ViewBoundary
							positionIndex={props.positionIndex}
							errorComponent={props.fallbackErrorComponent}
							fallbackErrorComponent={undefined}
						>
							<Error
								error={toBrowserPageError({
									kind: "render-failure",
									pattern: failure().pattern,
									cause: failure().cause,
								})}
							/>
						</ViewBoundary>
					</Show>
				);
			}}
		>
			{props.children}
		</Errored>
	);
}

function ViewOutlet(props: { readonly positionIndex: number }): JSX.Element {
	const frame = useContext(FrameProvider);
	const { defaultErrorComponent } = useAdapterCtx();
	const positionIndex = untrack(() => props.positionIndex);
	const errorComponents = createMemo(() => {
		const current = frame();
		return current === undefined
			? {
					positionErrorComponent: defaultErrorComponent,
					fallbackErrorComponent: undefined,
				}
			: selectErrorComponents({
					frame: current,
					defaultErrorComponent,
					positionIndex,
				});
	});
	function getPositionErrorComponent(): ErrorComponent {
		return errorComponents().positionErrorComponent;
	}
	function getFallbackErrorComponent(): ErrorComponent | undefined {
		return errorComponents().fallbackErrorComponent;
	}

	const failure = createMemo(() => {
		return frame()?.error?.positionIndex === positionIndex
			? frame()?.error
			: undefined;
	});
	const entry = createMemo(() => frame()?.entries[positionIndex]);
	const PositionError = dynamic(getPositionErrorComponent);
	function Outlet(): JSX.Element {
		return <ViewOutlet positionIndex={positionIndex + 1} />;
	}
	createEffect(
		() => frame(),
		(current) => {
			if (
				current !== undefined &&
				getDestinationDepth(current) === positionIndex
			) {
				consumeScrollIntent(current.revision);
			}
		},
	);
	return (
		<Show when={frame()}>
			<Show
				when={failure()}
				fallback={
					<ViewBoundary
						positionIndex={positionIndex}
						errorComponent={getPositionErrorComponent()}
						fallbackErrorComponent={getFallbackErrorComponent()}
					>
						<Show when={entry()?.pattern} keyed>
							{(_pattern) => {
								const View = dynamic(() => {
									return (entry()?.view?.component ??
										Outlet) as Component<
										Record<string, unknown>
									>;
								});
								return (
									<ViewBoundary
										positionIndex={positionIndex}
										errorComponent={undefined}
										fallbackErrorComponent={undefined}
									>
										<View
											pattern={entry()?.pattern}
											params={entry()?.params}
											splatValues={entry()?.splatValues}
											input={entry()?.input}
											serverOutput={entry()?.serverOutput}
											clientOutput={entry()?.clientOutput}
											Outlet={Outlet}
										/>
									</ViewBoundary>
								);
							}}
						</Show>
					</ViewBoundary>
				}
			>
				{(error) => {
					return (
						<ViewBoundary
							positionIndex={positionIndex}
							errorComponent={getFallbackErrorComponent()}
							fallbackErrorComponent={undefined}
						>
							<PositionError error={error().error} />
						</ViewBoundary>
					);
				}}
			</Show>
		</Show>
	);
}
