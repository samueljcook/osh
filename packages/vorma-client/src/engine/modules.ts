import {
	callWithResultAsync,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "primitives-ts/results";
import {
	checkRendererNeutralView,
	type RendererNeutralViewDef,
} from "../views.ts";

/*
View modules load through native dynamic `import`. A view file's default
export is one view or an array of views, and a position's pattern selects the
member. Production modules are cached by the platform; development retains the
namespaces that hot updates accepted.
*/

type ModuleLoadError = {
	readonly kind: "module-load-error";
	readonly url: string;
	readonly cause: unknown;
};

type ModuleNamespace = Readonly<Record<string, unknown>>;

type ModuleMode =
	| { readonly kind: "production" }
	| {
			readonly kind: "development";
			readonly modules: Map<string, ModuleNamespace>;
	  };
let mode: ModuleMode = { kind: "production" };

async function importModule(url: string): Promise<ModuleNamespace> {
	const current = mode;
	if (current.kind === "production") {
		return (await import(/* @vite-ignore */ url)) as ModuleNamespace;
	}
	const key = new URL(url, window.location.href).href;
	const existing = current.modules.get(key);
	if (existing !== undefined) {
		return existing;
	}
	const module = (await import(/* @vite-ignore */ url)) as ModuleNamespace;
	if (mode !== current) {
		return module;
	}
	if (!current.modules.has(key)) {
		current.modules.set(key, module);
	}
	return current.modules.get(key)!;
}

/** The views of a view file's default export. */
function listViews(exported: unknown): ReadonlyArray<RendererNeutralViewDef> {
	if (exported === undefined) {
		return [];
	}
	return (Array.isArray(exported) ? exported : [exported]).filter(
		checkRendererNeutralView,
	);
}

/**
 * Selects the view that has the pattern among a module's views. A module
 * without that view, and a view with a `staleTimeMillis` that is not a finite
 * positive number, are errors of the module.
 */
function selectView(input: {
	readonly views: ReadonlyArray<RendererNeutralViewDef>;
	readonly pattern: string;
}): Result<RendererNeutralViewDef, string> {
	const view = input.views.find(
		(candidate) => candidate.pattern === input.pattern,
	);
	if (view === undefined) {
		return createErrorResult(
			`The view module's default export has no view for ${input.pattern}`,
		);
	}
	const { staleTimeMillis } = view;
	if (
		staleTimeMillis !== undefined &&
		!(Number.isFinite(staleTimeMillis) && staleTimeMillis > 0)
	) {
		return createErrorResult(
			`The view for ${input.pattern} has a staleTimeMillis of ${String(staleTimeMillis)}, which is not a finite positive number of milliseconds`,
		);
	}
	return createSuccessResult(view);
}

/** Loads a position's module and selects the view that has the position's pattern. */
export async function loadView(input: {
	readonly url: string;
	readonly pattern: string;
}): Promise<Result<RendererNeutralViewDef, ModuleLoadError>> {
	const imported = await callWithResultAsync(() => importModule(input.url));
	if (imported.kind === "error") {
		return createErrorResult({
			kind: "module-load-error",
			url: input.url,
			cause: imported.error,
		});
	}
	const view = selectView({
		views: listViews(imported.value["default"]),
		pattern: input.pattern,
	});
	return view.kind === "error"
		? createErrorResult({
				kind: "module-load-error",
				url: input.url,
				cause: view.error,
			})
		: view;
}

/** Retains accepted replacement namespaces for development and maps each replaced view to its replacement. */
export function enableDevModules(): {
	readonly replaceModule: (input: {
		readonly url: string;
		readonly module: ModuleNamespace;
	}) => ReadonlyMap<unknown, unknown>;
	readonly close: () => void;
} {
	const modules = new Map<string, ModuleNamespace>();
	const replacements = new Map<unknown, unknown>();
	const installed: ModuleMode = { kind: "development", modules };
	mode = installed;
	return {
		replaceModule(input) {
			const url = new URL(input.url, window.location.href);
			url.searchParams.delete("t");
			const previous = modules.get(url.href);
			modules.set(url.href, input.module);
			/** Maps a value, and every earlier value that it replaced, to its replacement. */
			function replace(change: {
				readonly value: unknown;
				readonly replacement: unknown;
			}): void {
				for (const [old, current] of replacements) {
					if (current === change.value) {
						replacements.set(old, change.replacement);
					}
				}
				replacements.set(change.value, change.replacement);
			}
			for (const [name, value] of Object.entries(previous ?? {})) {
				if (name !== "default") {
					replace({ value, replacement: input.module[name] });
				}
			}
			// A position holds its selected view, which can be one member of an
			// array. Only the replacement's view with the same pattern replaces
			// it; a view has no replacement if the module does not export a
			// valid view for its pattern.
			const views = listViews(input.module["default"]);
			for (const view of listViews(previous?.["default"])) {
				const selected = selectView({ views, pattern: view.pattern });
				replace({
					value: view,
					replacement:
						selected.kind === "ok" ? selected.value : undefined,
				});
			}
			return replacements;
		},
		close() {
			if (mode === installed) {
				mode = { kind: "production" };
			}
		},
	};
}
