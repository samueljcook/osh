import {
	Component,
	useLayoutEffect,
	useMemo,
	useSyncExternalStore,
	type ComponentType,
	type ReactElement,
	type ReactNode,
} from "react";
import {
	consumeScrollIntent,
	getRenderFrame,
	reportRenderFailure,
	subscribeToRenderFrame,
	toBrowserPageError,
	type Frame,
	type RouteEntry,
} from "../adapter-api.ts";
import {
	attributeRenderFailure,
	createAttributedFailure,
	getDestinationDepth,
	selectErrorComponents,
	type RenderFailure,
} from "../adapter-shared/outlet.ts";
import type { PageError } from "../index.ts";
import { useAdapterCtx, type ErrorComponent } from "./context.ts";

/*
The outlet chain. One outlet component renders each depth of the committed
frame, and one class boundary carries React's catch protocol. A matched prefix
keeps its component identity because each depth renders the same component type
under a key that is the entry's pattern. `adapter-shared/outlet.ts` describes
which error component renders a failure.
*/

function subscribeToFrame(onChange: () => void): () => void {
	return subscribeToRenderFrame(() => onChange());
}

function useRenderFrame(): Frame | undefined {
	return useSyncExternalStore(subscribeToFrame, getRenderFrame);
}

/** The error that a component shows for a render failure that the frame does not carry yet. */
function toRenderFailureError(failure: RenderFailure): PageError {
	return toBrowserPageError({
		kind: "render-failure",
		pattern: failure.pattern,
		cause: failure.cause,
	});
}

type BoundaryProps = {
	readonly positionIndex: number;
	readonly pattern: string | undefined;
	readonly revision: number;
	/** What renders a caught failure; without one, the boundary attributes the failure and hands it on. */
	readonly errorComponent: ErrorComponent | undefined;
	/** What renders a failure of `errorComponent` itself at the same position. */
	readonly fallbackErrorComponent: ErrorComponent | undefined;
	readonly children: ReactNode;
};

type BoundaryState = {
	readonly revision: number;
	readonly caught:
		| { readonly kind: "clear" }
		| { readonly kind: "caught"; readonly cause: unknown };
};

class ViewBoundary extends Component<BoundaryProps, BoundaryState> {
	override state: BoundaryState = {
		revision: this.props.revision,
		caught: { kind: "clear" },
	};

	static getDerivedStateFromProps(
		props: BoundaryProps,
		state: BoundaryState,
	): BoundaryState | null {
		return props.revision === state.revision
			? null
			: { revision: props.revision, caught: { kind: "clear" } };
	}

	static getDerivedStateFromError(
		cause: unknown,
	): Pick<BoundaryState, "caught"> {
		return { caught: { kind: "caught", cause } };
	}

	/** The failure with the position it belongs to: a handed-on one keeps its own, a new one is this boundary's. */
	private attributeFailure(cause: unknown): RenderFailure {
		return attributeRenderFailure({
			caught: cause,
			positionIndex: this.props.positionIndex,
			pattern: this.props.pattern,
		});
	}

	override componentDidCatch(cause: unknown): void {
		// A boundary without presentation hands the failure on, and the boundary
		// that presents it reports it.
		if (this.props.errorComponent === undefined) {
			return;
		}
		const failure = this.attributeFailure(cause);
		reportRenderFailure({
			revision: this.props.revision,
			positionIndex: failure.positionIndex,
			pattern: failure.pattern,
			cause: failure.cause,
		});
	}

	override render(): ReactNode {
		if (this.state.caught.kind === "clear") {
			return this.props.children;
		}
		const failure = this.attributeFailure(this.state.caught.cause);
		const PositionErrorComponent = this.props.errorComponent;
		if (PositionErrorComponent === undefined) {
			// React's protocol for handing a failure to the next boundary is a throw.
			throw createAttributedFailure(failure);
		}
		return (
			<ViewBoundary
				positionIndex={this.props.positionIndex}
				pattern={this.props.pattern}
				revision={this.props.revision}
				errorComponent={this.props.fallbackErrorComponent}
				fallbackErrorComponent={undefined}
			>
				<PositionErrorComponent error={toRenderFailureError(failure)} />
			</ViewBoundary>
		);
	}
}

/** One position: its entry, or its failure in its error component. */
export function ViewOutlet(props: {
	readonly positionIndex: number;
}): ReactElement | null {
	const { defaultErrorComponent } = useAdapterCtx();
	const frame = useRenderFrame();
	const revision = frame?.revision;
	const destinationDepth =
		frame === undefined ? undefined : getDestinationDepth(frame);
	useLayoutEffect(() => {
		if (
			revision !== undefined &&
			destinationDepth === props.positionIndex
		) {
			consumeScrollIntent(revision);
		}
	}, [props.positionIndex, revision, destinationDepth]);
	if (frame === undefined) {
		return null;
	}
	const {
		positionErrorComponent: PositionErrorComponent,
		fallbackErrorComponent,
	} = selectErrorComponents({
		frame,
		defaultErrorComponent,
		positionIndex: props.positionIndex,
	});
	const failure =
		frame.error !== undefined &&
		frame.error.positionIndex === props.positionIndex
			? frame.error
			: undefined;
	// A failure caught in this slot belongs to this position, so it renders
	// here; a failure that the position already shows is kept.
	return (
		<ViewBoundary
			key={failure === undefined ? "children" : "failure"}
			positionIndex={props.positionIndex}
			pattern={frame.entries[props.positionIndex]?.pattern}
			revision={frame.revision}
			errorComponent={
				failure === undefined
					? PositionErrorComponent
					: fallbackErrorComponent
			}
			fallbackErrorComponent={
				failure === undefined ? fallbackErrorComponent : undefined
			}
		>
			{failure === undefined ? (
				<EntryAtPosition positionIndex={props.positionIndex} />
			) : (
				<PositionErrorComponent error={failure.error} />
			)}
		</ViewBoundary>
	);
}

/** Renders one successful position. Its own rendering failures propagate to the enclosing outlet's boundary. */
function EntryAtPosition(props: {
	readonly positionIndex: number;
}): ReactElement | null {
	const frame = useRenderFrame();
	const positionIndex = props.positionIndex;
	const entry = frame?.entries[positionIndex];
	const Outlet = useMemo(() => {
		return function Outlet(): ReactElement | null {
			return <ViewOutlet positionIndex={positionIndex + 1} />;
		};
	}, [positionIndex]);
	if (frame === undefined || entry === undefined) {
		return null;
	}
	if (entry.view === undefined) {
		// A view without a client definition renders its child.
		return <ViewOutlet positionIndex={positionIndex + 1} />;
	}
	return (
		<MatchedView
			key={entry.pattern}
			entry={entry}
			positionIndex={positionIndex}
			revision={frame.revision}
			view={entry.view}
			Outlet={Outlet}
		/>
	);
}

/** The component of a view that defines none: it renders only its `Outlet`. */
function PassThroughComponent(props: {
	readonly Outlet: ComponentType;
}): ReactElement {
	return <props.Outlet />;
}

type RenderedViewProps = Readonly<Record<string, unknown>>;

function MatchedView(props: {
	readonly entry: RouteEntry;
	readonly positionIndex: number;
	readonly revision: number;
	readonly view: NonNullable<RouteEntry["view"]>;
	readonly Outlet: ComponentType;
}): ReactElement {
	const { entry, Outlet } = props;
	const ViewComponent = (props.view.component ??
		PassThroughComponent) as ComponentType<RenderedViewProps>;
	// Every commit renders this outlet. While the entry is unchanged, it passes
	// the same element, so the view's component renders again only when a value that it
	// reads changes.
	const element = useMemo(() => {
		return (
			<ViewComponent
				pattern={entry.pattern}
				params={entry.params}
				splatValues={entry.splatValues}
				input={entry.input}
				serverOutput={entry.serverOutput}
				clientOutput={entry.clientOutput}
				Outlet={Outlet}
			/>
		);
	}, [ViewComponent, entry, Outlet]);
	return (
		<ViewBoundary
			positionIndex={props.positionIndex}
			pattern={entry.pattern}
			revision={props.revision}
			errorComponent={undefined}
			fallbackErrorComponent={undefined}
		>
			{element}
		</ViewBoundary>
	);
}
