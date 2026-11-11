import * as hooks from "preact/hooks";
import { createAdapterHooks } from "../adapter-shared/hooks.ts";
import { useSyncExternalStore } from "./external-store.ts";
export type {
	UseMutationOptions,
	UseUrlSyncOptions,
} from "../adapter-shared/hooks.ts";
const adapterHooks = createAdapterHooks({
	...hooks,
	useSyncExternalStore,
});

/**
 * Watches a query while the component is subscribed. The call is refreshed when
 * it changes and on revalidation; undefined disables fetching. The last settled
 * result remains available while a newer request is pending. Unmounting releases
 * this component's interest in shared work.
 *
 * The call must be structurally stable: Vorma compares each render's call with
 * the watched call by contents, and a call with other contents starts a fetch,
 * which renders the component again. Plain objects, arrays, maps, and sets
 * compare by contents, with map keys and set members by identity; `FormData`,
 * `Headers`, and `URLSearchParams` compare by their entries in order; and
 * `Date`, `URL`, and regular-expression values compare by their values. Any
 * other object, such as a class instance or a `File` in form data, compares by
 * identity, so a render must not create it anew. The comparison leaves out
 * `signal`: a call that differs from the watched call only in its signal
 * keeps the watched data and starts no fetch. The client uses the signal of
 * the latest call for every later fetch, including the fetches of
 * revalidations, so that signal must stay unaborted for as long as the
 * watcher should fetch: a fetch with an aborted signal ends at once and keeps
 * the earlier data. A signal with a deadline that each render creates, such
 * as one from `AbortSignal.timeout`, must not be used.
 */
export const useQuery = adapterHooks.useQuery;

/**
 * Returns a mutation function and reactive request state. Every mutate call
 * resolves with its own result; result follows the most recently started call,
 * and isPending includes every pending call from this hook. Mutation requests
 * use callApiEndpoint's automatic revalidation and cancellation semantics.
 */
export const useMutation = adapterHooks.useMutation;

/**
 * Synchronizes a page target while the component is mounted and enabled.
 * Another navigation, including history traversal, cancels a pending target.
 * Changing options alone does not restart navigation; the next target change
 * uses the options from that render. Unmounting stops synchronization. With
 * its default options, a synchronized navigation performs no scroll, so it
 * keeps the scroll position and focus, and an input that drives the target
 * keeps focus. With `scrollToTop` or `scrollToHash` true, focus follows the
 * scroll that a synchronized navigation performs, as `navigate` describes.
 */
export const useUrlSync = adapterHooks.useUrlSync;
