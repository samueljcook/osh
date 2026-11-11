import {
	createObservableValue,
	type ObservableValue,
} from "primitives-ts/observable-values";
import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "primitives-ts/results";
import {
	createElement,
	useCallback,
	useEffect,
	useMemo,
	useRef,
	useSyncExternalStore,
	type CSSProperties,
	type HTMLAttributes,
	type KeyboardEvent as ReactKeyboardEvent,
	type PointerEvent as ReactPointerEvent,
	type ReactElement,
} from "react";

export type ResizeEdge = "left" | "right" | "top" | "bottom";
export type ResizeCorner =
	| "top-left"
	| "top-right"
	| "bottom-left"
	| "bottom-right";

/** Reports an invalid numeric option; creation creates no group or region when it fails. */
export type ResizeConfigError = {
	readonly kind: "invalid-option";
	readonly option:
		| "handleSize"
		| "grabAreaExtension"
		| "hoverDelayMillis"
		| "minimumSize"
		| "maximumSize"
		| "initialSize"
		| "collapsedSize";
	readonly value: number;
};

/** Reports a non-finite size update; the region retains its previous size. */
export type ResizeSizeError = {
	readonly kind: "invalid-size";
	readonly size: number;
};

const FINE_KEYBOARD_STEP_FRACTION = 0.02;
const COARSE_KEYBOARD_STEP_FRACTION = 0.1;
const UNBOUNDED_FINE_KEYBOARD_STEP_PIXELS = 10;
const UNBOUNDED_COARSE_KEYBOARD_STEP_PIXELS = 50;

function createResizeValue<Value>(initialValue: Value): ObservableValue<Value> {
	return createObservableValue({
		initialValue,
		compareValuesForEqualityFn: ({ left, right }) => Object.is(left, right),
		onListenerErrorFn: (cause) => {
			globalThis.reportError(cause);
		},
	});
}

function useResizeValue<Value>(value: ObservableValue<Value>): Value {
	const subscribe = useCallback(
		(onStoreChange: () => void) => value.subscribe(onStoreChange),
		[value],
	);
	return useSyncExternalStore(subscribe, value.getValue, value.getValue);
}

type ResizeCursor = "ew-resize" | "move" | "ns-resize";

type ResizeAppearance = {
	handleSize: number;
	grabAreaExtension: number;
	shouldSetCursorStyle: boolean;
	hoverStyle: CSSProperties;
	dragStyle: CSSProperties;
};

export type ResizeGroupOptions = {
	/** Specifies the handle thickness in CSS pixels as a positive, finite number. */
	handleSize: number;
	/** Specifies the grab-area extension in CSS pixels on each side of the handle as a nonnegative, finite number. */
	grabAreaExtension: number;
	shouldSetCursorStyle: boolean;
	/** Specifies the hover delay as a finite duration from zero through 2,147,483,647 milliseconds. */
	hoverDelayMillis: number;
	hoverStyle?: CSSProperties;
	dragStyle?: CSSProperties;
};

export type ResizableRegionOptions = {
	edge: ResizeEdge;
	/** Specifies the initial size in CSS pixels as a finite number, which creation clamps to the region's bounds. */
	initialSize: number;
	/** Specifies the expanded lower bound in CSS pixels as a finite, nonnegative number. */
	minimumSize: number;
	/** Specifies the upper bound in CSS pixels; positive Infinity permits an unbounded size. */
	maximumSize: number;
	/** Permits collapsing the region to `collapsedSize` via `Enter`, `Home`, or double-click when true. */
	collapsible?: boolean;
	/** Specifies the collapsed size in CSS pixels as a finite, nonnegative number at most `minimumSize`; omitting this option uses 0 when `collapsible` is true. */
	collapsedSize?: number;
	handleSize?: number;
	grabAreaExtension?: number;
	shouldSetCursorStyle?: boolean;
	hoverStyle?: CSSProperties;
	dragStyle?: CSSProperties;
};

declare const resizableRegionDefTypeId: unique symbol;

export type ResizableRegionDef = {
	readonly [resizableRegionDefTypeId]: true;
	readonly edge: ResizeEdge;
};

export type ResizeGroup = {
	/** Creates an independent region with copied group appearance and explicit size bounds. */
	createResizableRegion(
		options: ResizableRegionOptions,
	): Result<ResizableRegionDef, ResizeConfigError>;
};

type InteractionTarget = {
	identity: object;
	regions: ReadonlyArray<ResizableRegionDef>;
	cursor: ResizeCursor;
	shouldSetCursorStyle: boolean;
};

type PendingHover = {
	target: InteractionTarget;
	/** Records the monotonic `performance.now()` timestamp in milliseconds when hover becomes visible. */
	activateAt: number;
	timeoutId: ReturnType<typeof globalThis.setTimeout>;
};

type HoverState =
	| { readonly kind: "none" }
	| { readonly kind: "waiting"; readonly pending: PendingHover }
	| { readonly kind: "visible"; readonly target: InteractionTarget }
	| {
			readonly kind: "replacing";
			readonly target: InteractionTarget;
			readonly pending: PendingHover;
	  };

type DragOwnership =
	| { readonly kind: "idle" }
	| { readonly kind: "dragging"; readonly drag: DragState };

/**
 * `setHoverState` makes every assignment to `hover` and publishes its visible
 * target to `hoveredTargetValue`. `setDragState` makes every assignment to
 * `drag` and publishes its target to `activeTargetValue`.
 */
type ResizeGroupState = {
	hoverDelayMillis: number;
	activeTargetValue: ObservableValue<InteractionTarget | null>;
	hoveredTargetValue: ObservableValue<InteractionTarget | null>;
	hover: HoverState;
	drag: DragOwnership;
};

type RegionCollapsePolicy =
	| { readonly kind: "fixed" }
	| {
			readonly kind: "collapsible";
			readonly collapsedSize: number;
			expansion:
				| { readonly kind: "expanded" }
				| {
						readonly kind: "collapsed";
						readonly restoreSize: number;
				  };
	  };

type ResizableRegionState = ResizeAppearance & {
	group: ResizeGroup;
	sizeValue: ObservableValue<number>;
	edge: ResizeEdge;
	minimumSize: number;
	maximumSize: number;
	collapse: RegionCollapsePolicy;
	cursor: "ew-resize" | "ns-resize";
};

type DragState = {
	target: InteractionTarget;
	element: HTMLDivElement;
	pointerId: number;
	positions: ReadonlyArray<{
		state: ResizableRegionState;
		startCoordinate: number;
		startSize: number;
	}>;
};

type RegisteredGrabArea = {
	groupState: ResizeGroupState;
	target: InteractionTarget;
};

const resizeGroupStates = new WeakMap<ResizeGroup, ResizeGroupState>();
const resizableRegionStates = new WeakMap<
	ResizableRegionDef,
	ResizableRegionState
>();
/**
 * Maps each mounted grab-area element to its handle. A grab area's ref callback
 * sets the entry when React attaches the element. An entry for a removed element
 * stays until garbage collection, but a removed element is never the related
 * target of a pointer event, so lookups never find it.
 */
const registeredGrabAreas = new WeakMap<Element, RegisteredGrabArea>();

/** Clamps a finite number between `minimum` and `maximum`; every caller verifies finiteness first. */
function clamp({
	value,
	minimum,
	maximum,
}: {
	value: number;
	minimum: number;
	maximum: number;
}) {
	return Math.min(Math.max(value, minimum), maximum);
}

function getResizeGroupState(group: ResizeGroup) {
	const state = resizeGroupStates.get(group);

	if (!state) {
		throw new Error("Unknown resize group");
	}

	return state;
}

function getResizableRegionState(region: ResizableRegionDef) {
	const state = resizableRegionStates.get(region);

	if (!state) {
		throw new Error("Unknown resizable region");
	}

	return state;
}

function testEdgeIsHorizontal(edge: ResizeEdge) {
	return edge === "left" || edge === "right";
}

function toResizeDirection(edge: ResizeEdge) {
	return edge === "right" || edge === "bottom" ? 1 : -1;
}

function toPointerCoordinate(edge: ResizeEdge, event: ReactPointerEvent) {
	return testEdgeIsHorizontal(edge) ? event.clientX : event.clientY;
}

function checkElementIsRtl(element: HTMLElement): boolean {
	return globalThis.getComputedStyle(element).direction === "rtl";
}

function applyRegionSizeUpdate({
	state,
	nextSize,
	allowSnapCollapse,
}: {
	state: ResizableRegionState;
	nextSize: number;
	allowSnapCollapse: boolean;
}): void {
	if (state.collapse.kind === "fixed") {
		state.sizeValue.setValue(
			clamp({
				value: nextSize,
				minimum: state.minimumSize,
				maximum: state.maximumSize,
			}),
		);
		return;
	}
	const { collapsedSize } = state.collapse;
	const collapseThreshold = allowSnapCollapse
		? (collapsedSize + state.minimumSize) / 2
		: collapsedSize;
	if (
		nextSize <= collapseThreshold &&
		(nextSize <= collapsedSize || collapsedSize < state.minimumSize)
	) {
		const previousSize = state.sizeValue.getValue();
		const restoreSize =
			state.collapse.expansion.kind === "collapsed"
				? state.collapse.expansion.restoreSize
				: clamp({
						value: Math.max(previousSize, state.minimumSize),
						minimum: state.minimumSize,
						maximum: state.maximumSize,
					});
		state.collapse.expansion = { kind: "collapsed", restoreSize };
		state.sizeValue.setValue(collapsedSize);
		return;
	}
	const clampedSize = clamp({
		value: nextSize,
		minimum: state.minimumSize,
		maximum: state.maximumSize,
	});
	state.collapse.expansion = { kind: "expanded" };
	state.sizeValue.setValue(clampedSize);
}

function deriveKeyboardStep({
	state,
	isCoarse,
}: {
	state: ResizableRegionState;
	isCoarse: boolean;
}): number {
	if (
		Number.isFinite(state.maximumSize) &&
		state.maximumSize > state.minimumSize
	) {
		const span = state.maximumSize - state.minimumSize;
		const fraction = isCoarse
			? COARSE_KEYBOARD_STEP_FRACTION
			: FINE_KEYBOARD_STEP_FRACTION;
		return Math.max(1, span * fraction);
	}
	return isCoarse
		? UNBOUNDED_COARSE_KEYBOARD_STEP_PIXELS
		: UNBOUNDED_FINE_KEYBOARD_STEP_PIXELS;
}

function stepRegionByKeyboard({
	state,
	signedStep,
}: {
	state: ResizableRegionState;
	signedStep: number;
}): void {
	const currentSize = state.sizeValue.getValue();
	if (
		state.collapse.kind === "collapsible" &&
		state.collapse.expansion.kind === "collapsed"
	) {
		if (signedStep <= 0) {
			return;
		}
		const expandedSize = clamp({
			value: Math.max(state.minimumSize, currentSize + signedStep),
			minimum: state.minimumSize,
			maximum: state.maximumSize,
		});
		state.collapse.expansion = { kind: "expanded" };
		state.sizeValue.setValue(expandedSize);
		return;
	}
	state.sizeValue.setValue(
		clamp({
			value: currentSize + signedStep,
			minimum: state.minimumSize,
			maximum: state.maximumSize,
		}),
	);
}

function jumpRegionToMinimum(state: ResizableRegionState): void {
	if (state.collapse.kind === "collapsible") {
		const previousSize = state.sizeValue.getValue();
		const restoreSize =
			state.collapse.expansion.kind === "collapsed"
				? state.collapse.expansion.restoreSize
				: clamp({
						value: Math.max(previousSize, state.minimumSize),
						minimum: state.minimumSize,
						maximum: state.maximumSize,
					});
		state.collapse.expansion = { kind: "collapsed", restoreSize };
		state.sizeValue.setValue(state.collapse.collapsedSize);
		return;
	}
	state.sizeValue.setValue(state.minimumSize);
}

function jumpRegionToMaximum(state: ResizableRegionState): void {
	if (Number.isFinite(state.maximumSize)) {
		if (state.collapse.kind === "collapsible") {
			state.collapse.expansion = { kind: "expanded" };
		}
		state.sizeValue.setValue(state.maximumSize);
		return;
	}
	if (
		state.collapse.kind === "collapsible" &&
		state.collapse.expansion.kind === "collapsed"
	) {
		const restoredSize = clamp({
			value: state.collapse.expansion.restoreSize,
			minimum: state.minimumSize,
			maximum: state.maximumSize,
		});
		state.collapse.expansion = { kind: "expanded" };
		state.sizeValue.setValue(restoredSize);
	}
}

function toggleRegionCollapse(state: ResizableRegionState): boolean {
	if (state.collapse.kind !== "collapsible") {
		return false;
	}
	if (state.collapse.expansion.kind === "collapsed") {
		const restoredSize = clamp({
			value: state.collapse.expansion.restoreSize,
			minimum: state.minimumSize,
			maximum: state.maximumSize,
		});
		state.collapse.expansion = { kind: "expanded" };
		state.sizeValue.setValue(restoredSize);
		return true;
	}
	const currentSize = state.sizeValue.getValue();
	const restoreSize = clamp({
		value: Math.max(currentSize, state.minimumSize),
		minimum: state.minimumSize,
		maximum: state.maximumSize,
	});
	state.collapse.expansion = { kind: "collapsed", restoreSize };
	state.sizeValue.setValue(state.collapse.collapsedSize);
	return true;
}

function toggleIntersectionCollapse(
	states: ReadonlyArray<ResizableRegionState>,
): boolean {
	const collapsibleStates = states.filter(
		(
			state,
		): state is ResizableRegionState & {
			collapse: Extract<RegionCollapsePolicy, { kind: "collapsible" }>;
		} => state.collapse.kind === "collapsible",
	);
	if (collapsibleStates.length === 0) {
		return false;
	}
	const hasCollapsedRegion = collapsibleStates.some(
		(state) => state.collapse.expansion.kind === "collapsed",
	);
	if (hasCollapsedRegion) {
		for (const state of collapsibleStates) {
			if (state.collapse.expansion.kind === "collapsed") {
				toggleRegionCollapse(state);
			}
		}
		return true;
	}
	for (const state of collapsibleStates) {
		toggleRegionCollapse(state);
	}
	return true;
}

function deriveAriaValueNow({
	state,
	size,
}: {
	state: ResizableRegionState;
	size: number;
}): number {
	const lowerBound =
		state.collapse.kind === "collapsible"
			? state.collapse.collapsedSize
			: state.minimumSize;
	if (Number.isFinite(state.maximumSize)) {
		const span = state.maximumSize - lowerBound;
		if (span <= 0) {
			return 0;
		}
		return clamp({
			value: Math.round(((size - lowerBound) / span) * 100),
			minimum: 0,
			maximum: 100,
		});
	}
	const offset = Math.max(0, size - lowerBound);
	if (offset === 0) {
		return 0;
	}
	const scale = Math.max(100, state.minimumSize);
	return clamp({
		value: Math.round((offset / (offset + scale)) * 100),
		minimum: 1,
		maximum: 99,
	});
}

function parseAppearance(appearance: {
	handleSize: number;
	grabAreaExtension: number;
}): Result<void, ResizeConfigError> {
	if (!Number.isFinite(appearance.handleSize) || appearance.handleSize <= 0) {
		return createErrorResult({
			kind: "invalid-option",
			option: "handleSize",
			value: appearance.handleSize,
		});
	}

	if (
		!Number.isFinite(appearance.grabAreaExtension) ||
		appearance.grabAreaExtension < 0
	) {
		return createErrorResult({
			kind: "invalid-option",
			option: "grabAreaExtension",
			value: appearance.grabAreaExtension,
		});
	}
	return createSuccessResult(undefined);
}

/**
 * Creates a resize group without starting timers. Pointer interactions and
 * mounted handles own event listeners and hover timers. The application chooses
 * bounds, geometry, hover delay, cursor behavior, and appearance. Numeric
 * configuration failures return ResizeConfigError. Appearance defaults are
 * copied at creation; later mutation of the supplied options does not change
 * the group or subsequently created regions. Applications using this entry must declare
 * `react` and `@types/react`; the renderer must support `useSyncExternalStore`.
 */
export function createResizeGroup(
	options: ResizeGroupOptions,
): Result<ResizeGroup, ResizeConfigError> {
	const appearanceResult = parseAppearance(options);
	if (appearanceResult.kind === "error") {
		return appearanceResult;
	}

	if (
		!Number.isFinite(options.hoverDelayMillis) ||
		options.hoverDelayMillis < 0 ||
		options.hoverDelayMillis > 2_147_483_647
	) {
		return createErrorResult({
			kind: "invalid-option",
			option: "hoverDelayMillis",
			value: options.hoverDelayMillis,
		});
	}
	const groupAppearance: ResizeAppearance = {
		handleSize: options.handleSize,
		grabAreaExtension: options.grabAreaExtension,
		shouldSetCursorStyle: options.shouldSetCursorStyle,
		hoverStyle: { ...options.hoverStyle },
		dragStyle: { ...options.dragStyle },
	};

	const groupState: ResizeGroupState = {
		hoverDelayMillis: options.hoverDelayMillis,
		activeTargetValue: createResizeValue<InteractionTarget | null>(null),
		hoveredTargetValue: createResizeValue<InteractionTarget | null>(null),
		hover: { kind: "none" },
		drag: { kind: "idle" },
	};

	const group: ResizeGroup = {
		createResizableRegion(regionOptions) {
			if (!Number.isFinite(regionOptions.initialSize)) {
				return createErrorResult({
					kind: "invalid-option",
					option: "initialSize",
					value: regionOptions.initialSize,
				});
			}
			if (
				!Number.isFinite(regionOptions.minimumSize) ||
				regionOptions.minimumSize < 0
			) {
				return createErrorResult({
					kind: "invalid-option",
					option: "minimumSize",
					value: regionOptions.minimumSize,
				});
			}
			if (
				Number.isNaN(regionOptions.maximumSize) ||
				regionOptions.maximumSize < regionOptions.minimumSize
			) {
				return createErrorResult({
					kind: "invalid-option",
					option: "maximumSize",
					value: regionOptions.maximumSize,
				});
			}
			if (
				regionOptions.collapsedSize !== undefined &&
				(!Number.isFinite(regionOptions.collapsedSize) ||
					regionOptions.collapsedSize < 0 ||
					regionOptions.collapsedSize > regionOptions.minimumSize)
			) {
				return createErrorResult({
					kind: "invalid-option",
					option: "collapsedSize",
					value: regionOptions.collapsedSize,
				});
			}

			const appearance: ResizeAppearance = {
				handleSize:
					regionOptions.handleSize ?? groupAppearance.handleSize,
				grabAreaExtension:
					regionOptions.grabAreaExtension ??
					groupAppearance.grabAreaExtension,
				shouldSetCursorStyle:
					regionOptions.shouldSetCursorStyle ??
					groupAppearance.shouldSetCursorStyle,
				hoverStyle: {
					...groupAppearance.hoverStyle,
					...regionOptions.hoverStyle,
				},
				dragStyle: {
					...groupAppearance.dragStyle,
					...regionOptions.dragStyle,
				},
			};

			const parsed = parseAppearance(appearance);
			if (parsed.kind === "error") {
				return parsed;
			}

			const collapsedSize = regionOptions.collapsedSize ?? 0;
			const startsCollapsed =
				regionOptions.collapsible === true &&
				regionOptions.initialSize <= collapsedSize;
			const expandedInitialSize = clamp({
				value: Math.max(
					regionOptions.initialSize,
					regionOptions.minimumSize,
				),
				minimum: regionOptions.minimumSize,
				maximum: regionOptions.maximumSize,
			});
			const initialSize = startsCollapsed
				? collapsedSize
				: clamp({
						value: regionOptions.initialSize,
						minimum: regionOptions.minimumSize,
						maximum: regionOptions.maximumSize,
					});
			const collapse: RegionCollapsePolicy =
				regionOptions.collapsible === true
					? {
							kind: "collapsible",
							collapsedSize,
							expansion: startsCollapsed
								? {
										kind: "collapsed",
										restoreSize: expandedInitialSize,
									}
								: { kind: "expanded" },
						}
					: { kind: "fixed" };

			const region = Object.freeze({
				edge: regionOptions.edge,
			}) as ResizableRegionDef;

			resizableRegionStates.set(region, {
				group,
				sizeValue: createResizeValue(initialSize),
				edge: regionOptions.edge,
				minimumSize: regionOptions.minimumSize,
				maximumSize: regionOptions.maximumSize,
				collapse,
				cursor: testEdgeIsHorizontal(regionOptions.edge)
					? "ew-resize"
					: "ns-resize",
				...appearance,
			});

			return createSuccessResult(region);
		},
	};

	resizeGroupStates.set(group, groupState);

	return createSuccessResult(group);
}

function testTargetsShareRegion({
	first,
	second,
}: {
	first: InteractionTarget;
	second: InteractionTarget;
}) {
	return first.regions.some((region) => second.regions.includes(region));
}

function toVisibleHoverTarget(hover: HoverState): InteractionTarget | null {
	return hover.kind === "visible" || hover.kind === "replacing"
		? hover.target
		: null;
}

function getHoveredTarget(
	groupState: ResizeGroupState,
): InteractionTarget | null {
	return toVisibleHoverTarget(groupState.hover);
}

function getPendingHover(groupState: ResizeGroupState): PendingHover | null {
	const hover = groupState.hover;
	return hover.kind === "waiting" || hover.kind === "replacing"
		? hover.pending
		: null;
}

function setHoverState(groupState: ResizeGroupState, hover: HoverState) {
	groupState.hover = hover;
	groupState.hoveredTargetValue.setValue(toVisibleHoverTarget(hover));
}

function cancelPendingHover(groupState: ResizeGroupState) {
	const hover = groupState.hover;
	switch (hover.kind) {
		case "waiting":
			globalThis.clearTimeout(hover.pending.timeoutId);
			setHoverState(groupState, { kind: "none" });
			return;
		case "replacing":
			globalThis.clearTimeout(hover.pending.timeoutId);
			setHoverState(groupState, {
				kind: "visible",
				target: hover.target,
			});
			return;
		case "none":
		case "visible":
			return;
	}
}

function commitHover(
	groupState: ResizeGroupState,
	target: InteractionTarget | null,
) {
	cancelPendingHover(groupState);
	setHoverState(
		groupState,
		target === null ? { kind: "none" } : { kind: "visible", target },
	);
}

function scheduleHover(
	groupState: ResizeGroupState,
	target: InteractionTarget,
	activateAt: number,
) {
	const remainingDelay = Math.max(0, activateAt - performance.now());
	if (remainingDelay === 0) {
		commitHover(groupState, target);
		return;
	}
	const previous = getHoveredTarget(groupState);
	const pending: PendingHover = {
		target,
		activateAt,
		timeoutId: globalThis.setTimeout(() => {
			if (getPendingHover(groupState) !== pending) {
				return;
			}
			setHoverState(groupState, { kind: "visible", target });
		}, remainingDelay),
	};
	setHoverState(
		groupState,
		previous === null
			? { kind: "waiting", pending }
			: { kind: "replacing", target: previous, pending },
	);
}

function requestHover(groupState: ResizeGroupState, target: InteractionTarget) {
	const currentTarget = getHoveredTarget(groupState);
	if (currentTarget?.identity === target.identity) {
		cancelPendingHover(groupState);
		return;
	}
	if (
		currentTarget &&
		testTargetsShareRegion({ first: currentTarget, second: target })
	) {
		commitHover(groupState, target);
		return;
	}
	const pendingHover = getPendingHover(groupState);
	if (
		pendingHover &&
		testTargetsShareRegion({ first: pendingHover.target, second: target })
	) {
		const activateAt = pendingHover.activateAt;
		cancelPendingHover(groupState);
		scheduleHover(groupState, target, activateAt);
		return;
	}
	cancelPendingHover(groupState);
	scheduleHover(
		groupState,
		target,
		performance.now() + groupState.hoverDelayMillis,
	);
}

function findRegisteredGrabArea(target: EventTarget | null) {
	let element = target instanceof Element ? target : null;

	while (element) {
		const registered = registeredGrabAreas.get(element);

		if (registered) {
			return registered;
		}

		element = element.parentElement;
	}

	return undefined;
}

function leaveGrabArea(
	groupState: ResizeGroupState,
	target: InteractionTarget,
	relatedTarget: EventTarget | null,
) {
	if (groupState.drag.kind !== "idle") {
		return;
	}
	const nextGrabArea = findRegisteredGrabArea(relatedTarget);
	const nextTarget =
		nextGrabArea?.groupState === groupState
			? nextGrabArea.target
			: undefined;
	const currentTarget = getHoveredTarget(groupState);
	const pendingHover = getPendingHover(groupState);

	if (pendingHover?.target.identity === target.identity) {
		if (
			nextTarget !== undefined &&
			nextTarget.identity !== currentTarget?.identity &&
			testTargetsShareRegion({
				first: pendingHover.target,
				second: nextTarget,
			})
		) {
			return;
		}
		if (
			currentTarget !== null &&
			nextTarget !== undefined &&
			(nextTarget.identity === currentTarget.identity ||
				testTargetsShareRegion({
					first: currentTarget,
					second: nextTarget,
				}))
		) {
			cancelPendingHover(groupState);
			return;
		}
		commitHover(groupState, null);
		return;
	}

	if (currentTarget?.identity !== target.identity) {
		return;
	}

	if (
		nextTarget !== undefined &&
		testTargetsShareRegion({
			first: currentTarget,
			second: nextTarget,
		})
	) {
		return;
	}

	commitHover(groupState, null);
}

function setDragState(groupState: ResizeGroupState, drag: DragOwnership) {
	groupState.drag = drag;
	groupState.activeTargetValue.setValue(
		drag.kind === "dragging" ? drag.drag.target : null,
	);
}

function clearTargetInteraction({
	groupState,
	target,
}: {
	groupState: ResizeGroupState;
	target: InteractionTarget;
}): void {
	const ownership = groupState.drag;
	if (
		ownership.kind === "dragging" &&
		ownership.drag.target.identity === target.identity
	) {
		const { pointerId, element } = ownership.drag;
		setDragState(groupState, { kind: "idle" });
		if (element.hasPointerCapture(pointerId)) {
			element.releasePointerCapture(pointerId);
		}
	}
	if (getPendingHover(groupState)?.target.identity === target.identity) {
		cancelPendingHover(groupState);
	}
	if (getHoveredTarget(groupState)?.identity === target.identity) {
		commitHover(groupState, null);
	}
}

function useGrabAreaInteraction({
	groupState,
	target,
	isDisabled,
}: {
	groupState: ResizeGroupState;
	target: InteractionTarget;
	isDisabled: boolean;
}) {
	function finishDragging(pointerId: number) {
		const ownership = groupState.drag;
		if (
			ownership.kind !== "dragging" ||
			ownership.drag.pointerId !== pointerId ||
			ownership.drag.target.identity !== target.identity
		) {
			return;
		}
		setDragState(groupState, { kind: "idle" });
		const { element } = ownership.drag;
		if (element.hasPointerCapture(pointerId)) {
			element.releasePointerCapture(pointerId);
		}
	}

	useEffect(() => {
		if (isDisabled) {
			clearTargetInteraction({ groupState, target });
		}
	}, [groupState, isDisabled, target]);

	const registerGrabArea = useCallback(
		(element: HTMLDivElement | null) => {
			if (element) {
				registeredGrabAreas.set(element, { groupState, target });
				return;
			}
			clearTargetInteraction({ groupState, target });
		},
		[groupState, target],
	);

	return {
		ref: registerGrabArea,
		onPointerEnter: () => {
			if (!isDisabled && groupState.drag.kind === "idle") {
				requestHover(groupState, target);
			}
		},
		onPointerLeave: (event: ReactPointerEvent<HTMLDivElement>) => {
			if (!isDisabled && groupState.drag.kind === "idle") {
				leaveGrabArea(groupState, target, event.relatedTarget);
			}
		},
		onPointerDown: (event: ReactPointerEvent<HTMLDivElement>) => {
			if (
				isDisabled ||
				!event.isPrimary ||
				event.button !== 0 ||
				groupState.drag.kind !== "idle"
			) {
				return;
			}
			const element = event.currentTarget;
			const pointerId = event.pointerId;
			commitHover(groupState, target);
			const captured = callWithResult(() => {
				return element.setPointerCapture(pointerId);
			});
			if (captured.kind === "error") {
				globalThis.reportError(captured.error);
				return;
			}
			setDragState(groupState, {
				kind: "dragging",
				drag: {
					target,
					element,
					pointerId,
					positions: target.regions.map((region) => {
						const state = getResizableRegionState(region);
						return {
							state,
							startCoordinate: toPointerCoordinate(
								state.edge,
								event,
							),
							startSize: state.sizeValue.getValue(),
						};
					}),
				},
			});
		},
		onPointerMove: (event: ReactPointerEvent<HTMLDivElement>) => {
			if (isDisabled) {
				return;
			}
			const ownership = groupState.drag;
			if (
				ownership.kind !== "dragging" ||
				ownership.drag.target.identity !== target.identity ||
				ownership.drag.pointerId !== event.pointerId
			) {
				return;
			}
			for (const { state, startCoordinate, startSize } of ownership.drag
				.positions) {
				const delta =
					toPointerCoordinate(state.edge, event) - startCoordinate;
				applyRegionSizeUpdate({
					state,
					nextSize: startSize + delta * toResizeDirection(state.edge),
					allowSnapCollapse: true,
				});
			}
		},
		onPointerUp: (event: ReactPointerEvent<HTMLDivElement>) =>
			finishDragging(event.pointerId),
		onPointerCancel: (event: ReactPointerEvent<HTMLDivElement>) =>
			finishDragging(event.pointerId),
		onLostPointerCapture: (event: ReactPointerEvent<HTMLDivElement>) =>
			finishDragging(event.pointerId),
	};
}

function deriveRegionStyle(edge: ResizeEdge, size: number): CSSProperties {
	if (testEdgeIsHorizontal(edge)) {
		return { position: "relative", width: size };
	}

	return { position: "relative", height: size };
}

function deriveGrabAreaStyle({
	state,
	isTargetActive,
	isTargetHovered,
	isExcludedByOtherDrag,
}: {
	state: ResizableRegionState;
	isTargetActive: boolean;
	isTargetHovered: boolean;
	isExcludedByOtherDrag: boolean;
}): CSSProperties {
	const grabSize = state.handleSize + state.grabAreaExtension * 2;
	const shared: CSSProperties = {
		position: "absolute",
		zIndex: 1,
		touchAction: "none",
		userSelect: "none",
	};

	if (state.shouldSetCursorStyle && (isTargetActive || isTargetHovered)) {
		shared.cursor = state.cursor;
	}

	if (isExcludedByOtherDrag) {
		shared.pointerEvents = "none";
	}

	switch (state.edge) {
		case "left":
			return {
				...shared,
				left: 0,
				top: 0,
				width: grabSize,
				height: "100%",
				transform: "translateX(-50%)",
			};
		case "right":
			return {
				...shared,
				right: 0,
				top: 0,
				width: grabSize,
				height: "100%",
				transform: "translateX(50%)",
			};
		case "top":
			return {
				...shared,
				left: 0,
				top: 0,
				width: "100%",
				height: grabSize,
				transform: "translateY(-50%)",
			};
		case "bottom":
			return {
				...shared,
				left: 0,
				bottom: 0,
				width: "100%",
				height: grabSize,
				transform: "translateY(50%)",
			};
	}
}

function mergeVisibleHandleStyles({
	state,
	style,
	isRegionHovered,
	isRegionActive,
}: {
	state: ResizableRegionState;
	style: CSSProperties | undefined;
	isRegionHovered: boolean;
	isRegionActive: boolean;
}): CSSProperties {
	const shared: CSSProperties = {
		position: "absolute",
		pointerEvents: "none",
		...style,
		...(isRegionHovered ? state.hoverStyle : undefined),
		...(isRegionActive ? state.dragStyle : undefined),
	};

	if (testEdgeIsHorizontal(state.edge)) {
		return {
			...shared,
			left: "50%",
			top: 0,
			width: state.handleSize,
			height: "100%",
			transform: "translateX(-50%)",
		};
	}

	return {
		...shared,
		left: 0,
		top: "50%",
		width: "100%",
		height: state.handleSize,
		transform: "translateY(-50%)",
	};
}

function deriveIntersectionGrabAreaStyle({
	corner,
	width,
	height,
	shouldSetCursorStyle,
	isTargetActive,
	isTargetHovered,
	isExcludedByOtherDrag,
	cursor,
}: {
	corner: ResizeCorner;
	width: number;
	height: number;
	shouldSetCursorStyle: boolean;
	isTargetActive: boolean;
	isTargetHovered: boolean;
	isExcludedByOtherDrag: boolean;
	cursor: ResizeCursor;
}): CSSProperties {
	const shared: CSSProperties = {
		position: "absolute",
		zIndex: 2,
		width,
		height,
		touchAction: "none",
		userSelect: "none",
	};

	if (shouldSetCursorStyle && (isTargetActive || isTargetHovered)) {
		shared.cursor = cursor;
	}

	if (isExcludedByOtherDrag) {
		shared.pointerEvents = "none";
	}

	switch (corner) {
		case "top-left":
			return {
				...shared,
				left: 0,
				top: 0,
				transform: "translate(-50%, -50%)",
			};
		case "top-right":
			return {
				...shared,
				right: 0,
				top: 0,
				transform: "translate(50%, -50%)",
			};
		case "bottom-left":
			return {
				...shared,
				left: 0,
				bottom: 0,
				transform: "translate(-50%, 50%)",
			};
		case "bottom-right":
			return {
				...shared,
				right: 0,
				bottom: 0,
				transform: "translate(50%, 50%)",
			};
	}
}

/** Reports whether any mounted handle in the group owns an active pointer drag. */
export function useIsResizeGroupActive(group: ResizeGroup) {
	const { activeTargetValue } = getResizeGroupState(group);
	return useResizeValue(activeTargetValue) !== null;
}

/** Returns the size and a setter that clamps finite values and returns ResizeSizeError for non-finite values. */
export function useResizableRegionSize(region: ResizableRegionDef) {
	const state = getResizableRegionState(region);
	const size = useResizeValue(state.sizeValue);
	const setSize = useCallback(
		(nextSize: number): Result<void, ResizeSizeError> => {
			if (!Number.isFinite(nextSize)) {
				return createErrorResult({
					kind: "invalid-size",
					size: nextSize,
				});
			}
			applyRegionSizeUpdate({
				state,
				nextSize,
				allowSnapCollapse: false,
			});
			return createSuccessResult(undefined);
		},
		[state],
	);

	return [size, setSize] as const;
}

export type ResizeGroupRootProps = HTMLAttributes<HTMLDivElement> & {
	group: ResizeGroup;
};

/**
 * Renders a group container with the active or hovered handle's cursor.
 * Place the group's regions inside this container to apply the cursor across
 * their shared area. Other HTML attributes, styles, and children are forwarded.
 */
export function ResizeGroupRoot({
	group,
	style,
	children,
	...props
}: ResizeGroupRootProps): ReactElement {
	const state = getResizeGroupState(group);
	const activeTarget = useResizeValue(state.activeTargetValue);
	const hoveredTarget = useResizeValue(state.hoveredTargetValue);
	const currentTarget = activeTarget ?? hoveredTarget;
	const cursor = currentTarget?.shouldSetCursorStyle
		? currentTarget.cursor
		: undefined;

	const isDragging = activeTarget !== null;
	const rootStyle: CSSProperties | undefined =
		cursor !== undefined || isDragging
			? {
					...style,
					...(cursor !== undefined ? { cursor } : undefined),
					...(isDragging ? { userSelect: "none" } : undefined),
				}
			: style;

	return createElement("div", { ...props, style: rootStyle }, children);
}

export type ResizableRegionProps = HTMLAttributes<HTMLDivElement> & {
	region: ResizableRegionDef;
};

/**
 * Renders a positioned region with its current width or height. The region's
 * managed dimension and relative positioning take precedence over supplied
 * styles. Mount ResizeHandle inside the region to resize its configured edge.
 */
export function ResizableRegion({
	region,
	style,
	children,
	...props
}: ResizableRegionProps): ReactElement {
	const state = getResizableRegionState(region);
	const size = useResizeValue(state.sizeValue);

	return createElement(
		"div",
		{
			...props,
			style: { ...style, ...deriveRegionStyle(state.edge, size) },
		},
		children,
	);
}

export type ResizeHandleProps = {
	region: ResizableRegionDef;
	className?: string;
	style?: CSSProperties;
	disabled?: boolean;
	"aria-controls"?: string;
	"aria-label"?: string;
	"aria-labelledby"?: string;
};

/**
 * Renders a WAI-ARIA 1.2 `separator` grab area and its visible handle for one
 * region. The outer grab area extends around the configured edge; `className`
 * and `style` apply to the inner visible handle.
 *
 * Under WAI-ARIA 1.2 `separator` semantics, `aria-orientation` describes the
 * orientation of the divider bar itself: a handle on a `"left"` or `"right"`
 * edge divides horizontally arranged panes and has `aria-orientation="vertical"`,
 * whereas a handle on a `"top"` or `"bottom"` edge divides vertically stacked
 * panes and has `aria-orientation="horizontal"`.
 *
 * When `disabled` is not true, the handle is keyboard-focusable (`tabIndex={0}`):
 * unmodified arrow keys along the resize axis adjust the size by 2% of a bounded
 * range (or 10 CSS pixels when `maximumSize` is unbounded), `Shift+Arrow` adjusts
 * by 10% (or 50 CSS pixels), `Home` jumps to `minimumSize` (or `collapsedSize`
 * when `collapsible` is true), `End` jumps to `maximumSize` when `maximumSize`
 * is finite (or restores a collapsed unbounded region to its pre-collapse size),
 * and `Enter` or double-click toggles collapse and restoration when the region
 * is `collapsible`. Horizontal arrow keys reverse direction when the computed
 * CSS `direction` of the handle is `"rtl"`. Unmounting releases pointer capture
 * and removes this handle's active, hover, and pending-hover state.
 */
export function ResizeHandle({
	region,
	className,
	style,
	disabled,
	"aria-controls": ariaControls,
	"aria-label": ariaLabel,
	"aria-labelledby": ariaLabelledBy,
}: ResizeHandleProps): ReactElement {
	const isDisabled = disabled === true;
	const state = getResizableRegionState(region);
	const size = useResizeValue(state.sizeValue);
	const groupState = getResizeGroupState(state.group);
	const activeTarget = useResizeValue(groupState.activeTargetValue);
	const hoveredTarget = useResizeValue(groupState.hoveredTargetValue);
	const identity = useRef<object>({}).current;
	const target = useMemo<InteractionTarget>(() => {
		return {
			identity,
			regions: [region],
			cursor: state.cursor,
			shouldSetCursorStyle: !isDisabled && state.shouldSetCursorStyle,
		};
	}, [
		identity,
		isDisabled,
		region,
		state.cursor,
		state.shouldSetCursorStyle,
	]);
	const interaction = useGrabAreaInteraction({
		groupState,
		target,
		isDisabled,
	});
	const isHorizontalEdge = testEdgeIsHorizontal(state.edge);
	const isRegionActive =
		!isDisabled && (activeTarget?.regions.includes(region) ?? false);
	const isRegionHovered =
		!isDisabled && (hoveredTarget?.regions.includes(region) ?? false);
	const isTargetActive = !isDisabled && activeTarget?.identity === identity;
	const isTargetHovered = !isDisabled && hoveredTarget?.identity === identity;

	function handleKeyDown(event: ReactKeyboardEvent<HTMLDivElement>): void {
		if (isDisabled || event.altKey || event.ctrlKey || event.metaKey) {
			return;
		}
		if (
			isHorizontalEdge &&
			(event.key === "ArrowLeft" || event.key === "ArrowRight")
		) {
			event.preventDefault();
			const rawDirection = event.key === "ArrowRight" ? 1 : -1;
			const axisDirection = checkElementIsRtl(event.currentTarget)
				? -rawDirection
				: rawDirection;
			const step = deriveKeyboardStep({
				state,
				isCoarse: event.shiftKey,
			});
			stepRegionByKeyboard({
				state,
				signedStep:
					axisDirection * step * toResizeDirection(state.edge),
			});
			return;
		}
		if (
			!isHorizontalEdge &&
			(event.key === "ArrowUp" || event.key === "ArrowDown")
		) {
			event.preventDefault();
			const axisDirection = event.key === "ArrowDown" ? 1 : -1;
			const step = deriveKeyboardStep({
				state,
				isCoarse: event.shiftKey,
			});
			stepRegionByKeyboard({
				state,
				signedStep:
					axisDirection * step * toResizeDirection(state.edge),
			});
			return;
		}
		if (event.key === "Home") {
			event.preventDefault();
			jumpRegionToMinimum(state);
			return;
		}
		if (event.key === "End") {
			event.preventDefault();
			jumpRegionToMaximum(state);
			return;
		}
		if (event.key === "Enter" && toggleRegionCollapse(state)) {
			event.preventDefault();
		}
	}

	function handleDoubleClick(): void {
		if (!isDisabled) {
			toggleRegionCollapse(state);
		}
	}

	const grabAreaStyle = deriveGrabAreaStyle({
		state,
		isTargetActive,
		isTargetHovered,
		isExcludedByOtherDrag: activeTarget !== null && !isRegionActive,
	});
	const visibleHandleStyle = mergeVisibleHandleStyles({
		state,
		style,
		isRegionHovered,
		isRegionActive,
	});

	return createElement(
		"div",
		{
			...interaction,
			role: "separator",
			tabIndex: isDisabled ? undefined : 0,
			"aria-disabled": isDisabled ? true : undefined,
			"aria-orientation": isHorizontalEdge ? "vertical" : "horizontal",
			"aria-valuemin": 0,
			"aria-valuemax": 100,
			"aria-valuenow": deriveAriaValueNow({ state, size }),
			"aria-controls": ariaControls,
			"aria-label": ariaLabel,
			"aria-labelledby": ariaLabelledBy,
			onKeyDown: handleKeyDown,
			onDoubleClick: handleDoubleClick,
			style: grabAreaStyle,
		},
		createElement("div", { className, style: visibleHandleStyle }),
	);
}

export type ResizeIntersectionHandleProps = {
	regions: readonly [ResizableRegionDef, ResizableRegionDef];
	corner: ResizeCorner;
	disabled?: boolean;
	"aria-label"?: string;
	"aria-labelledby"?: string;
};

/**
 * Renders one grab area with `role="application"` and
 * `aria-roledescription="2D resize handle"` that resizes two perpendicular
 * regions together. Callers should supply `aria-label` or `aria-labelledby` to
 * name the handle for assistive technologies. Both regions must belong to the
 * same group and use different axes; violating either requirement throws. The
 * corner positions the grab area relative to its containing element. When
 * `disabled` is not true, `ArrowLeft` and `ArrowRight` resize the horizontal
 * region (accounting for RTL), `ArrowUp` and `ArrowDown` resize the vertical
 * region, `Shift+Arrow` uses a coarse step, and `Enter` or double-click toggles
 * collapse on collapsible intersecting regions. Unmounting removes its
 * interaction state.
 */
export function ResizeIntersectionHandle({
	regions,
	corner,
	disabled,
	"aria-label": ariaLabel,
	"aria-labelledby": ariaLabelledBy,
}: ResizeIntersectionHandleProps): ReactElement {
	const isDisabled = disabled === true;
	const [firstRegion, secondRegion] = regions;
	const first = getResizableRegionState(firstRegion);
	const second = getResizableRegionState(secondRegion);

	if (first.group !== second.group) {
		throw new Error("Intersecting regions must belong to the same group");
	}

	if (
		testEdgeIsHorizontal(first.edge) === testEdgeIsHorizontal(second.edge)
	) {
		throw new Error("Intersecting regions must use different axes");
	}

	const groupState = getResizeGroupState(first.group);
	const activeTarget = useResizeValue(groupState.activeTargetValue);
	const hoveredTarget = useResizeValue(groupState.hoveredTargetValue);
	const identity = useRef<object>({}).current;
	const cursor = "move";
	const shouldSetCursorStyle =
		!isDisabled &&
		first.shouldSetCursorStyle &&
		second.shouldSetCursorStyle;
	const target = useMemo<InteractionTarget>(() => {
		return {
			identity,
			regions: [firstRegion, secondRegion],
			cursor,
			shouldSetCursorStyle,
		};
	}, [cursor, firstRegion, secondRegion, shouldSetCursorStyle, identity]);
	const interaction = useGrabAreaInteraction({
		groupState,
		target,
		isDisabled,
	});
	const isTargetActive = !isDisabled && activeTarget?.identity === identity;
	const isTargetHovered = !isDisabled && hoveredTarget?.identity === identity;
	const horizontal = testEdgeIsHorizontal(first.edge) ? first : second;
	const vertical = testEdgeIsHorizontal(first.edge) ? second : first;
	const width = horizontal.handleSize + horizontal.grabAreaExtension * 2;
	const height = vertical.handleSize + vertical.grabAreaExtension * 2;

	function handleKeyDown(event: ReactKeyboardEvent<HTMLDivElement>): void {
		if (isDisabled || event.altKey || event.ctrlKey || event.metaKey) {
			return;
		}
		if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
			event.preventDefault();
			const rawDirection = event.key === "ArrowRight" ? 1 : -1;
			const axisDirection = checkElementIsRtl(event.currentTarget)
				? -rawDirection
				: rawDirection;
			const step = deriveKeyboardStep({
				state: horizontal,
				isCoarse: event.shiftKey,
			});
			stepRegionByKeyboard({
				state: horizontal,
				signedStep:
					axisDirection * step * toResizeDirection(horizontal.edge),
			});
			return;
		}
		if (event.key === "ArrowUp" || event.key === "ArrowDown") {
			event.preventDefault();
			const axisDirection = event.key === "ArrowDown" ? 1 : -1;
			const step = deriveKeyboardStep({
				state: vertical,
				isCoarse: event.shiftKey,
			});
			stepRegionByKeyboard({
				state: vertical,
				signedStep:
					axisDirection * step * toResizeDirection(vertical.edge),
			});
			return;
		}
		if (
			event.key === "Enter" &&
			toggleIntersectionCollapse([horizontal, vertical])
		) {
			event.preventDefault();
		}
	}

	function handleDoubleClick(): void {
		if (!isDisabled) {
			toggleIntersectionCollapse([horizontal, vertical]);
		}
	}

	return createElement("div", {
		...interaction,
		role: "application",
		"aria-roledescription": "2D resize handle",
		tabIndex: isDisabled ? undefined : 0,
		"aria-disabled": isDisabled ? true : undefined,
		"aria-label": ariaLabel,
		"aria-labelledby": ariaLabelledBy,
		onKeyDown: handleKeyDown,
		onDoubleClick: handleDoubleClick,
		style: deriveIntersectionGrabAreaStyle({
			corner,
			width,
			height,
			shouldSetCursorStyle,
			isTargetActive,
			isTargetHovered,
			isExcludedByOtherDrag: activeTarget !== null && !isTargetActive,
			cursor,
		}),
	});
}
