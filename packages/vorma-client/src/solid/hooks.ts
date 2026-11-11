import {
	createEffect,
	createSignal,
	onCleanup,
	onSettled,
	untrack,
	type Accessor,
} from "solid-js";
import { pickDefinedFields } from "../adapter-shared/defined-fields.ts";
import { createQueryHolder } from "../adapter-shared/hooks.ts";
import type {
	MutationCallOf,
	MutateParams,
	RuntimeUrlSyncOptions,
	UseMutationOptions,
	UseUrlSyncOptions,
} from "../adapter-shared/hooks.ts";
import {
	createMutationState,
	readMutationResult,
	startMutation,
	settleMutation,
	type MutationState,
} from "../adapter-shared/mutation-state.ts";
import {
	callApiEndpoint,
	createUrlSync,
	toHref,
	type ApiEndpointCallResult,
	type MutationApiEndpointCall,
	type MutationResult,
	type NavTarget,
	type PageTarget,
	type QueryApiEndpointCall,
	type ViewPattern,
	type WatchedQueryState,
} from "../index.ts";
export type {
	UseMutationOptions,
	UseUrlSyncOptions,
} from "../adapter-shared/hooks.ts";

/**
 * Watches a reactive query call: it fetches when the owner settles, again
 * when the call changes, and again on every revalidation, until Solid disposes
 * the owner. An undefined call disables fetching. Read `result` and
 * `isFetching` inside JSX or a reactive computation, not by destructuring
 * them into snapshots. A new call that equals the watched call by contents,
 * apart from `signal`, keeps the watched data and starts no fetch. The client
 * uses the signal of the latest call for every later fetch, including the
 * fetches of revalidations, so that signal must stay unaborted for as long as
 * the watcher should fetch: a fetch with an aborted signal ends at once and
 * keeps the earlier data. A signal with a deadline that each run of the
 * accessor creates, such as one from `AbortSignal.timeout`, must not be used.
 * Plain objects, arrays, maps, and sets compare by contents, with map keys and set members by identity; `FormData`,
 * `Headers`, and `URLSearchParams` compare by their entries in order; `Date`,
 * `URL`, and regular-expression values compare by their values; and any other
 * object, such as a class instance or a `File` in form data, compares by
 * identity.
 *
 * ```tsx
 * const users = useQuery(() => ({
 * 	kind: "query",
 * 	method: "GET",
 * 	pattern: "/api/users",
 * 	input: { name_prefix: namePrefix() },
 * }));
 * ```
 *
 * The endpoint pattern and input in the example belong to an application and
 * must match its generated contracts.
 */
export function useQuery<const Call extends QueryApiEndpointCall>(
	call: Accessor<Call | undefined>,
): {
	readonly result: ApiEndpointCallResult<Call> | undefined;
	readonly isFetching: boolean;
	readonly refetch: () => void;
} {
	const [state, setState] = createSignal<WatchedQueryState>(
		{ result: undefined, isFetching: untrack(call) !== undefined },
		{ ownedWrite: true },
	);
	const holder = createQueryHolder(untrack(call));
	onSettled(() => {
		const stop = holder.subscribe(() => setState(holder.getSnapshot()));
		setState(holder.getSnapshot());
		return stop;
	});
	createEffect(call, holder.setCall);
	return {
		get result() {
			return state().result as ApiEndpointCallResult<Call> | undefined;
		},
		get isFetching() {
			return state().isFetching;
		},
		refetch: holder.refetch,
	};
}

/**
 * A mutation endpoint with state. `mutate` sends a call as `callApiEndpoint`
 * would and resolves with the same result. `result` follows the most recently
 * started call, and `isPending` is true while any call from this hook is in
 * flight; read both inside JSX or a reactive computation. The hook reads the
 * options object that it received when it was created.
 */
export function useMutation<const Options extends UseMutationOptions>(
	options: Options,
): {
	readonly mutate: (
		...args: MutateParams<Options>
	) => Promise<MutationResult<MutationCallOf<Options>>>;
	readonly result: MutationResult<MutationCallOf<Options>> | undefined;
	readonly isPending: boolean;
} {
	const [state, setState] = createSignal<MutationState>(
		createMutationState(),
	);
	// A call that settles after Solid disposed the owner changes no state.
	let lifetime: "owned" | "disposed" = "owned";
	onCleanup(() => {
		lifetime = "disposed";
	});
	async function mutate(
		args?: object,
	): Promise<MutationResult<MutationCallOf<Options>>> {
		const call = {};
		if (lifetime === "owned") {
			setState((previous) => startMutation(previous, call));
		}
		const result = (await callApiEndpoint({
			...options,
			...args,
			kind: "mutation",
		} as MutationApiEndpointCall)) as MutationResult<
			MutationCallOf<Options>
		>;
		if (lifetime === "owned") {
			setState((previous) => settleMutation(previous, { call, result }));
		}
		return result;
	}
	return {
		mutate: mutate as (
			...args: MutateParams<Options>
		) => Promise<MutationResult<MutationCallOf<Options>>>,
		get result() {
			return readMutationResult(state()) as
				| MutationResult<MutationCallOf<Options>>
				| undefined;
		},
		get isPending() {
			return state().kind === "active";
		},
	};
}

/**
 * Keeps the browser URL in step with a reactive page target until Solid
 * disposes the owner. A navigation that anything else starts, including Back
 * and Forward, cancels the pending target, and the synchronization never
 * undoes it. A change of an option alone starts no new synchronization; the
 * next target change uses the options that it reads then. With its default
 * options, a synchronized navigation performs no scroll, so it keeps the
 * scroll position and focus, and an input that drives the target keeps focus.
 * With `scrollToTop` or `scrollToHash` true, focus follows the scroll that a
 * synchronized navigation performs, as `navigate` describes.
 *
 * ```tsx
 * useUrlSync(() => ({
 * 	pattern: "/_index",
 * 	input: { name_prefix: namePrefix() },
 * 	debounceMillis: 300,
 * }));
 * ```
 *
 * The page pattern and input in the example belong to an application and must
 * match its generated contracts.
 */
export function useUrlSync<const Pattern extends ViewPattern>(
	options: Accessor<UseUrlSyncOptions<Pattern>>,
): void {
	type SyncState =
		| { readonly kind: "dormant" }
		| { readonly kind: "disabled"; readonly identity: string }
		| {
				readonly kind: "active";
				readonly identity: string;
				readonly synchronization: ReturnType<typeof createUrlSync>;
		  }
		| { readonly kind: "stopped" };
	let state: SyncState = { kind: "dormant" };
	createEffect(
		() => {
			const value = options() as RuntimeUrlSyncOptions;
			const resolved = toHref(value as NavTarget);
			return {
				value,
				identity:
					value.enabled === false
						? "disabled"
						: resolved.kind === "href"
							? `href\0${resolved.href}`
							: `invalid\0${resolved.msg}`,
			};
		},
		({ value, identity }) => {
			const previous = state;
			if (
				previous.kind === "stopped" ||
				(previous.kind !== "dormant" && previous.identity === identity)
			) {
				return;
			}
			if (previous.kind === "active") {
				previous.synchronization.stop();
			}
			if (value.enabled === false) {
				state = { kind: "disabled", identity };
				return;
			}
			const {
				enabled: _enabled,
				debounceMillis,
				replace,
				scrollToTop,
				scrollToHash,
				skipWorkIndicator,
				...target
			} = value;
			const synchronization = createUrlSync(
				pickDefinedFields({
					debounceMillis,
					replace,
					scrollToTop,
					scrollToHash,
					skipWorkIndicator,
				}),
			);
			state = { kind: "active", identity, synchronization };
			synchronization.setTarget(target as PageTarget);
		},
	);
	onCleanup(() => {
		const previous = state;
		state = { kind: "stopped" };
		if (previous.kind === "active") {
			previous.synchronization.stop();
		}
	});
}
