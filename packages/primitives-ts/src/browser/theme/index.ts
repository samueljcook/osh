import {
	createObservableValue,
	type ObservableValueChange,
} from "../../observable-values/index.ts";
import { createResourceScope } from "../../resource-scopes/index.ts";
import { callWithResult, type Result } from "../../results/index.ts";

/** The stored selection; `system` follows the browser's preferred color scheme. */
export type ThemePreference = "light" | "dark" | "system";

/** The selected preference and the light/dark value applied to the document. */
export type ThemeState = {
	readonly preference: ThemePreference;
	readonly resolvedTheme: "light" | "dark";
};

/** Use identical configuration for the pre-paint script and browser installation. */
export type ThemeConfig = {
	readonly storageKey: string;
	/** The preference to use when storage has no valid selection or cannot be read initially. */
	readonly defaultPreference: ThemePreference;
};

/** A failed browser-storage operation retains its original cause. */
export type ThemeStorageError = {
	readonly kind: "storage-error";
	readonly operation: "read" | "write";
	readonly cause: unknown;
};

/** Automatic failures are reported without converting their causes to strings. */
export type ThemeRuntimeError =
	| ThemeStorageError
	| { readonly kind: "listener-error"; readonly cause: unknown }
	| { readonly kind: "cleanup-error"; readonly cause: unknown };

/** Installation rejects a second owner of the same window's root theme classes. */
export type ThemeInstallError =
	| { readonly kind: "already-installed" }
	| { readonly kind: "browser-error"; readonly cause: unknown };

/** One installation owns the document's theme state and its browser listeners. */
export type ThemeController = {
	readonly getState: () => ThemeState;
	/**
	 * Applies the preference even if persistence fails. A storage error means the
	 * page changed but the selection was not saved. Listener-error-handler failures
	 * propagate as defects, as they do for observable values.
	 */
	readonly setPreference: (
		preference: ThemePreference,
	) => Result<void, ThemeStorageError | { readonly kind: "disposed" }>;
	/** Subscribes to future changes. After disposal, registration has no effect. */
	readonly subscribe: (
		listener: (change: ObservableValueChange<ThemeState>) => void,
	) => () => void;
	/** Removes all listeners; the last applied classes and stored preference remain. */
	readonly dispose: () => void;
};

const DARK_MEDIA_QUERY = "(prefers-color-scheme: dark)";
const THEME_CLASSES = ["light", "dark", "system"] as const;
const installedWindows = new WeakSet<Window>();

/** Returns the next selection in the system, light, dark, system cycle. */
export function getNextThemePreference(
	preference: ThemePreference,
): ThemePreference {
	switch (preference) {
		case "system":
			return "light";
		case "light":
			return "dark";
		case "dark":
			return "system";
	}
}

function parseStoredPreference(
	value: string | null,
): ThemePreference | undefined {
	return value === "light" || value === "dark" || value === "system"
		? value
		: undefined;
}

/**
 * Installs light/dark/system theme management on the supplied window. The root
 * element receives `light` or `dark`, plus `system` when that preference is
 * selected. Other classes are preserved; applications supply their own CSS.
 *
 * Only the preference is stored. System changes, storage events, and page
 * restoration resolve the current theme without persisting derived state.
 * Storage events re-read the current stored value rather than applying stale
 * event payloads. Failed automatic reads retain the current preference and go to
 * `onErrorFn`; the initial failed read uses `defaultPreference` and is reported.
 * Explicit writes report persistence failure through their returned Result.
 * The error handler and subscribers must be synchronous.
 *
 * Imports perform no browser work. Insert the matching pre-paint script before
 * stylesheets to apply the theme before this runtime module loads.
 */
export function installTheme(input: {
	readonly window: Window;
	readonly config: ThemeConfig;
	readonly onErrorFn: (error: ThemeRuntimeError) => void;
}): Result<ThemeController, ThemeInstallError> {
	const targetWindow = input.window;
	if (installedWindows.has(targetWindow)) {
		return { kind: "error", error: { kind: "already-installed" } };
	}
	const { storageKey, defaultPreference } = input.config;
	const scope = createResourceScope({
		onCleanupErrorFn: (cause) =>
			input.onErrorFn({ kind: "cleanup-error", cause }),
	});
	installedWindows.add(targetWindow);
	scope.addCleanup(() => installedWindows.delete(targetWindow));

	function readPreference(): Result<ThemePreference, ThemeStorageError> {
		const stored = callWithResult(() =>
			targetWindow.localStorage.getItem(storageKey),
		);
		return stored.kind === "ok"
			? {
					kind: "ok",
					value:
						parseStoredPreference(stored.value) ??
						defaultPreference,
				}
			: {
					kind: "error",
					error: {
						kind: "storage-error",
						operation: "read",
						cause: stored.error,
					},
				};
	}

	const installed = callWithResult(() => {
		const mediaQuery = targetWindow.matchMedia(DARK_MEDIA_QUERY);
		function resolveState(preference: ThemePreference): ThemeState {
			return Object.freeze({
				preference,
				resolvedTheme:
					preference === "system"
						? mediaQuery.matches
							? "dark"
							: "light"
						: preference,
			});
		}
		function applyClasses(state: ThemeState): void {
			const classes = targetWindow.document.documentElement.classList;
			classes.remove(...THEME_CLASSES);
			classes.add(state.preference);
			if (state.preference === "system") {
				classes.add(state.resolvedTheme);
			}
		}
		const initialPreference = readPreference();
		const state = createObservableValue<ThemeState>({
			initialValue: resolveState(
				initialPreference.kind === "ok"
					? initialPreference.value
					: defaultPreference,
			),
			compareValuesForEqualityFn: ({ left, right }) => {
				return (
					left.preference === right.preference &&
					left.resolvedTheme === right.resolvedTheme
				);
			},
			onListenerErrorFn: (cause) =>
				input.onErrorFn({ kind: "listener-error", cause }),
		});
		function applyPreference(preference: ThemePreference): void {
			const next = resolveState(preference);
			applyClasses(next);
			state.setValue(next);
		}
		function refreshStoredPreference(): void {
			if (scope.signal.aborted) {
				return;
			}
			const preference = readPreference();
			if (preference.kind === "error") {
				applyPreference(state.getValue().preference);
				input.onErrorFn(preference.error);
			} else {
				applyPreference(preference.value);
			}
		}
		function handleSystemChange(): void {
			if (
				!scope.signal.aborted &&
				state.getValue().preference === "system"
			) {
				applyPreference("system");
			}
		}
		function handleStorage(event: StorageEvent): void {
			if (
				scope.signal.aborted ||
				(event.key !== null && event.key !== storageKey)
			) {
				return;
			}
			const storage = callWithResult(() => targetWindow.localStorage);
			if (storage.kind === "error") {
				input.onErrorFn({
					kind: "storage-error",
					operation: "read",
					cause: storage.error,
				});
			} else if (event.storageArea === storage.value) {
				refreshStoredPreference();
			}
		}
		applyClasses(state.getValue());
		mediaQuery.addEventListener("change", handleSystemChange);
		scope.addCleanup(() =>
			mediaQuery.removeEventListener("change", handleSystemChange),
		);
		targetWindow.addEventListener("storage", handleStorage);
		scope.addCleanup(() =>
			targetWindow.removeEventListener("storage", handleStorage),
		);
		targetWindow.addEventListener("pageshow", refreshStoredPreference);
		scope.addCleanup(() => {
			targetWindow.removeEventListener(
				"pageshow",
				refreshStoredPreference,
			);
		});

		const controller: ThemeController = {
			getState: state.getValue,
			setPreference: (preference) => {
				if (scope.signal.aborted) {
					return { kind: "error", error: { kind: "disposed" } };
				}
				const saved = callWithResult(() =>
					targetWindow.localStorage.setItem(storageKey, preference),
				);
				applyPreference(preference);
				return saved.kind === "ok"
					? { kind: "ok", value: undefined }
					: {
							kind: "error",
							error: {
								kind: "storage-error",
								operation: "write",
								cause: saved.error,
							},
						};
			},
			subscribe: (listener) => {
				const unsubscribe = state.subscribe(listener);
				const removeCleanup = scope.addCleanup(unsubscribe);
				return () => {
					removeCleanup();
					unsubscribe();
				};
			},
			dispose: () => {
				scope.dispose();
			},
		};
		return { controller, initialPreference };
	});
	if (installed.kind === "error") {
		scope.dispose();
		return {
			kind: "error",
			error: { kind: "browser-error", cause: installed.error },
		};
	}
	if (installed.value.initialPreference.kind === "error") {
		const error = installed.value.initialPreference.error;
		const reported = callWithResult(() => input.onErrorFn(error));
		if (reported.kind === "error") {
			scope.dispose();
			throw reported.error;
		}
	}
	return { kind: "ok", value: installed.value.controller };
}

/**
 * Creates HTML-script-safe JavaScript text for an inline script before stylesheets.
 * The script reads local storage and applies root classes without writing storage.
 * Missing, invalid, or inaccessible storage uses the explicit default preference.
 * The application supplies the script element and its CSP authorization. Rust's
 * `primitives_rs::browser::theme::pre_paint_script` emits the same script.
 */
export function createThemePrePaintScript(config: ThemeConfig): string {
	function encodeScriptString(value: string): string {
		return JSON.stringify(value).replace(
			/[<>&\u2028\u2029]/gu,
			(character) =>
				`\\u${character.charCodeAt(0).toString(16).padStart(4, "0")}`,
		);
	}
	return `(function(){
const storageKey=${encodeScriptString(config.storageKey)};
let preference=${encodeScriptString(config.defaultPreference)};
try{const stored=window.localStorage.getItem(storageKey);if(stored==="light"||stored==="dark"||stored==="system")preference=stored;}catch{}
const resolved=preference==="system"?(window.matchMedia("(prefers-color-scheme: dark)").matches?"dark":"light"):preference;
const classes=window.document.documentElement.classList;
classes.remove("light","dark","system");classes.add(preference);if(preference==="system")classes.add(resolved);
})();`;
}
