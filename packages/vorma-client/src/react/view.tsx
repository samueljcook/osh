import type { ComponentType } from "react";
import { brandRendererNeutralView } from "../adapter-api.ts";
import type {
	BeforePageCommitFn,
	ClientLoaderCtx,
	ViewChildDeclaredErrorOf,
	PageError,
	ViewPattern,
} from "../index.ts";
import type { RendererNeutralViewProps } from "../views.ts";

/** What the view's component receives. `ClientOutput` is the output of the view's own client loader. */
export type ViewProps<Pattern extends ViewPattern, ClientOutput> = Omit<
	RendererNeutralViewProps<Pattern, ComponentType>,
	"clientOutput"
> & { readonly clientOutput: ClientOutput };

export type ViewOptions<Pattern extends ViewPattern> = {
	readonly pattern: Pattern;
	/** Renders in place of a failed immediate child, inside this view's `Outlet`, whether or not the view defines a component. */
	readonly childErrorComponent?: ComponentType<{
		readonly error: PageError<ViewChildDeclaredErrorOf<Pattern>>;
	}>;
	readonly beforePageCommit?: BeforePageCommitFn;
	/**
	 * Retains a successful navigation result for this many milliseconds after
	 * its commit when this view is the destination leaf, so that a navigation
	 * to the same destination within that time reuses it. Omission retains
	 * nothing. A value that is not a finite positive number is an error of
	 * the view's module: a navigation that needs the view fails at the stage
	 * `modules`, and boot on such a page fails. Revalidation, invalidation,
	 * build refusal, and development updates retire retained results early.
	 * Reuse preserves the original preparation time.
	 */
	readonly staleTimeMillis?: number;
	/**
	 * True runs the view's client loader again, over the page's existing
	 * server output, when a development update replaces the view's module.
	 * Omission keeps the loader's existing output.
	 */
	readonly rerunClientLoaderOnHmr?: boolean;
};

/**
 * A view as the build and the client read it. The build reads the pattern and
 * the client loader's output from this type, so they appear in it as the
 * definition gave them.
 */
export type ViewDef<Pattern extends ViewPattern, ClientOutput> = {
	readonly pattern: Pattern;
	/** Absent for a pass-through view, which renders only its `Outlet`. */
	readonly component?: ComponentType<never>;
	readonly childErrorComponent?: ComponentType<never>;
	readonly clientLoader?: (ctx: never) => Promise<ClientOutput>;
	readonly beforePageCommit?: BeforePageCommitFn;
	/**
	 * Retains a successful navigation result for this many milliseconds after
	 * its commit when this view is the destination leaf, so that a navigation
	 * to the same destination within that time reuses it. Omission retains
	 * nothing. A value that is not a finite positive number is an error of
	 * the view's module: a navigation that needs the view fails at the stage
	 * `modules`, and boot on such a page fails. Revalidation, invalidation,
	 * build refusal, and development updates retire retained results early.
	 * Reuse preserves the original preparation time.
	 */
	readonly staleTimeMillis?: number;
	/**
	 * True runs the view's client loader again, over the page's existing
	 * server output, when a development update replaces the view's module.
	 * Omission keeps the loader's existing output.
	 */
	readonly rerunClientLoaderOnHmr?: boolean;
};

/**
 * Defines the view for one pattern. A view file default-exports one view or an
 * array of views, and the build finds them there.
 *
 * A definition without a `component` is a pass-through view: the client renders
 * its `Outlet` in place of the view's component, so the view is a nested UI
 * segment that adds no markup. Its client loader, `beforePageCommit`,
 * `staleTimeMillis`, `rerunClientLoaderOnHmr`, and `childErrorComponent`
 * work as they do for a view with a component.
 *
 * A client loader can run for a read that the server then rejects, for example
 * because of search input that the server refuses. Vorma discards the output of
 * such a loader.
 *
 * In development, when `vorma dev` replaces a view's module, the client
 * replaces the view in the committed page without a request: the view's
 * component mounts again, and the views above it keep their component state.
 * A navigation in flight starts again, so that it commits the replaced view.
 * An update of other modules goes to the renderer's own refresh integration
 * when the application configures one. When one update changes both view
 * modules and modules of that refresh integration, the page reloads, so that
 * every module uses the same module identities; an update that no module
 * accepts, such as one that reaches the application's entry, also reloads the
 * page. A reload keeps no component state.
 */
export function createView<const Pattern extends ViewPattern, ClientOutput>(
	def: ViewOptions<Pattern> & {
		readonly clientLoader: (
			ctx: ClientLoaderCtx<Pattern>,
		) => Promise<ClientOutput>;
		readonly component?: ComponentType<
			ViewProps<Pattern, NoInfer<ClientOutput>>
		>;
	},
): ViewDef<Pattern, ClientOutput>;
export function createView<const Pattern extends ViewPattern>(
	def: ViewOptions<Pattern> & {
		readonly clientLoader?: never;
		readonly component?: ComponentType<ViewProps<Pattern, undefined>>;
	},
): ViewDef<Pattern, undefined>;
export function createView(def: object): object {
	return brandRendererNeutralView(def);
}
