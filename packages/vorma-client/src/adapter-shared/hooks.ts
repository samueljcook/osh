import {
	callApiEndpoint,
	createUrlSync,
	toHref,
	watchQuery,
	type ApiEndpointCallResult,
	type MutationApiEndpointCall,
	type MutationResult,
	type NavTarget,
	type PageTarget,
	type QueryApiEndpointCall,
	type ViewPattern,
	type UrlSyncOptions,
	type WatchedQuery,
	type WatchedQueryState,
} from "../index.ts";
import { pickDefinedFields } from "./defined-fields.ts";
import {
	createMutationState,
	readMutationResult,
	startMutation,
	settleMutation,
	type MutationState,
} from "./mutation-state.ts";

// Queries

/** The part of a watched query that a hook adapter reads: a snapshot, a subscription, and the commands. */
type QueryHolder = {
	readonly subscribe: (onChange: () => void) => () => void;
	readonly getSnapshot: () => WatchedQueryState;
	readonly setCall: (call: QueryApiEndpointCall | undefined) => void;
	readonly refetch: () => void;
};

/**
 * Starts watching when a hook adapter subscribes and stops when it unsubscribes, so a
 * remount (as in StrictMode) starts a new watch instead of reviving a stopped
 * one. A hook subscribes at most once at a time. Before the subscription, the
 * snapshot says that a call is fetching.
 */
export function createQueryHolder(
	initialCall: QueryApiEndpointCall | undefined,
): QueryHolder {
	type Subscription =
		| { readonly kind: "unsubscribed" }
		| { readonly kind: "subscribed"; readonly watcher: WatchedQuery };
	let call = initialCall;
	let subscription: Subscription = { kind: "unsubscribed" };
	let snapshot: WatchedQueryState = {
		result: undefined,
		isFetching: initialCall !== undefined,
	};
	function adopt(state: WatchedQueryState): void {
		if (
			state.result !== snapshot.result ||
			state.isFetching !== snapshot.isFetching
		) {
			snapshot = state;
		}
	}
	return {
		subscribe: (onChange) => {
			const watcher = watchQuery(call);
			subscription = { kind: "subscribed", watcher };
			adopt(watcher.getState());
			const stopListening = watcher.subscribe((state) => {
				adopt(state);
				onChange();
			});
			return () => {
				subscription = { kind: "unsubscribed" };
				stopListening();
				watcher.stop();
			};
		},
		getSnapshot: () => snapshot,
		setCall: (next) => {
			call = next;
			if (subscription.kind === "subscribed") {
				subscription.watcher.setCall(next);
			}
		},
		refetch: () => {
			if (subscription.kind === "subscribed") {
				subscription.watcher.refetch();
			}
		},
	};
}

// Mutations

type DistributiveOmit<Value, Key extends PropertyKey> = Value extends unknown
	? Omit<Value, Key>
	: never;

/** The options that a mutation hook takes: the endpoint and the settings that all of its calls share. */
export type UseMutationOptions = DistributiveOmit<
	MutationApiEndpointCall,
	| "kind"
	| "params"
	| "splatValues"
	| "input"
	| "formInput"
	| "signal"
	| "requestOptions"
>;

type MutationApiEndpointPattern = MutationApiEndpointCall["pattern"];

export type MutationCallOf<
	Options extends { method: string; pattern: string },
> = [string] extends [MutationApiEndpointPattern]
	? MutationApiEndpointCall
	: Extract<
			MutationApiEndpointCall,
			{
				readonly method: Options["method"];
				readonly pattern: Options["pattern"];
			}
		>;

/** What `mutate` takes: the parts of the call that the hook's options do not fix. */
type MutateArgs<Options extends { method: string; pattern: string }> =
	DistributiveOmit<
		MutationCallOf<Options>,
		| "kind"
		| "method"
		| "pattern"
		| "supersedeKey"
		| "skipWorkIndicator"
		| "skipAutoRevalidate"
	>;

export type MutateParams<Options extends { method: string; pattern: string }> =
	{} extends MutateArgs<Options>
		? [args?: MutateArgs<Options>]
		: [args: MutateArgs<Options>];

// URL synchronization

export type UseUrlSyncOptions<Pattern extends ViewPattern> =
	PageTarget<Pattern> &
		UrlSyncOptions & {
			/** False stops the synchronization; true when omitted. */
			readonly enabled?: boolean;
		};

/** `UseUrlSyncOptions` with the fields of every target variant, as the implementation reads them. */
export type RuntimeUrlSyncOptions = UrlSyncOptions & {
	readonly enabled?: boolean | undefined;
	readonly pattern?: string | undefined;
	readonly params?: Readonly<Record<string, string>> | undefined;
	readonly splatValues?: ReadonlyArray<string> | undefined;
	readonly input?: unknown;
	readonly hash?: string | undefined;
};

export type HookRuntime = {
	readonly useRef: <T>(initial: T) => { current: T };
	readonly useState: <T>(
		initial: T | (() => T),
	) => [T, (next: T | ((previous: T) => T)) => void];
	readonly useEffect: (
		effect: () => void | (() => void),
		deps?: ReadonlyArray<unknown>,
	) => void;
	readonly useLayoutEffect: (
		effect: () => void | (() => void),
		deps?: ReadonlyArray<unknown>,
	) => void;
	readonly useCallback: <T extends Function>(
		callback: T,
		deps: ReadonlyArray<unknown>,
	) => T;
	readonly useSyncExternalStore: <T>(
		subscribe: (listener: () => void) => () => void,
		getSnapshot: () => T,
	) => T;
};

/** Binds Vorma's hook behavior to a renderer's subscription and cleanup APIs. */
export function createAdapterHooks(runtime: HookRuntime) {
	const {
		useCallback,
		useEffect,
		useLayoutEffect,
		useRef,
		useState,
		useSyncExternalStore,
	} = runtime;
	/**
	 * Watches a query endpoint: it fetches when the component mounts, again when
	 * the call changes, and again on every revalidation. `result` is the most
	 * recent settled result and stays while a newer request is in flight. A
	 * `undefined` call fetches nothing. The adapters' `useQuery` documentation
	 * states how calls compare.
	 */
	function useQuery<const Call extends QueryApiEndpointCall>(
		call: Call | undefined,
	): {
		readonly result: ApiEndpointCallResult<Call> | undefined;
		readonly isFetching: boolean;
		readonly refetch: () => void;
	} {
		const [holder] = useState(() => createQueryHolder(call));
		const state = useSyncExternalStore(
			holder.subscribe,
			holder.getSnapshot,
		);
		// An unchanged call starts nothing, so this runs after every render.
		useEffect(() => {
			holder.setCall(call);
		});
		return {
			result: state.result as ApiEndpointCallResult<Call> | undefined,
			isFetching: state.isFetching,
			refetch: holder.refetch,
		};
	}

	/**
	 * A mutation endpoint with state. `mutate` sends a call as `callApiEndpoint`
	 * would and resolves with the same result. `result` follows the most recently
	 * started call, and `isPending` is true while any call from this hook is in
	 * flight.
	 */
	function useMutation<const Options extends UseMutationOptions>(
		options: Options,
	): {
		readonly mutate: (
			...args: MutateParams<Options>
		) => Promise<MutationResult<MutationCallOf<Options>>>;
		readonly result: MutationResult<MutationCallOf<Options>> | undefined;
		readonly isPending: boolean;
	} {
		const [state, setState] = useState<MutationState>(createMutationState);
		const latestOptions = useRef(options);
		useLayoutEffect(() => {
			latestOptions.current = options;
		});
		const mutate = useCallback(
			async (
				args?: object,
			): Promise<MutationResult<MutationCallOf<Options>>> => {
				const call = {};
				setState((previous) => startMutation(previous, call));
				const result = (await callApiEndpoint({
					...latestOptions.current,
					...args,
					kind: "mutation",
				} as MutationApiEndpointCall)) as MutationResult<
					MutationCallOf<Options>
				>;
				setState((previous) =>
					settleMutation(previous, { call, result }),
				);
				return result;
			},
			[],
		);
		return {
			mutate: mutate as (
				...args: MutateParams<Options>
			) => Promise<MutationResult<MutationCallOf<Options>>>,
			result: readMutationResult(state) as
				| MutationResult<MutationCallOf<Options>>
				| undefined,
			isPending: state.kind === "active",
		};
	}

	/**
	 * Keeps the browser URL in step with a target while the component is mounted.
	 * A navigation that anything else starts, including Back and Forward, cancels
	 * the pending target, and the synchronization never undoes it.
	 */
	function useUrlSync<const Pattern extends ViewPattern>(
		options: UseUrlSyncOptions<Pattern>,
	): void {
		const {
			enabled,
			debounceMillis,
			replace,
			scrollToTop,
			scrollToHash,
			skipWorkIndicator,
			...target
		} = options as RuntimeUrlSyncOptions;
		const resolved = toHref(target as NavTarget);
		// A target that resolves to the same href needs no new synchronization.
		const targetIdentity =
			resolved.kind === "href"
				? `href\0${resolved.href}`
				: `invalid\0${resolved.msg}`;
		// A change of an option alone starts no new synchronization, because that
		// would navigate again to a target that the application already left. The
		// next synchronization reads the options of the render that started it.
		const latest = useRef({
			target,
			syncOptions: pickDefinedFields({
				debounceMillis,
				replace,
				scrollToTop,
				scrollToHash,
				skipWorkIndicator,
			}),
		});
		useLayoutEffect(() => {
			latest.current = {
				target,
				syncOptions: pickDefinedFields({
					debounceMillis,
					replace,
					scrollToTop,
					scrollToHash,
					skipWorkIndicator,
				}),
			};
		});
		useEffect(() => {
			if (enabled === false) {
				return;
			}
			const synchronization = createUrlSync(latest.current.syncOptions);
			synchronization.setTarget(latest.current.target as PageTarget);
			return () => synchronization.stop();
		}, [enabled, targetIdentity]);
	}

	return { useQuery, useMutation, useUrlSync };
}
