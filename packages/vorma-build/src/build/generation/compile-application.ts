import { Order, Result } from "effect";
import {
	createNestedMatcher,
	type CompiledNestedPattern,
} from "primitives-ts/path-matcher";
import type {
	InspectedClientView,
	InspectedEndpoint,
	InspectedProjects,
	InspectedViewLoader,
} from "../inspection/inspect-projects.ts";
import { createNestedRouteChains } from "../patterns/nested-route-chains.ts";
import {
	compileViewPattern,
	formatRoutePatternError,
} from "../patterns/route-patterns.ts";
import { testGeneratedClientIdentifier } from "./render-client-contracts.ts";

/*
The static application model: the server's declarations, from the Rust
server's contract, and the client's views, joined by pattern. The model checks
what the build needs to generate types: that each view pattern compiles, that
no two client view files have patterns at one matching position, as the path
matcher defines a position, and that no server type name collides with a
generated name. The Rust server checks the rest of the
routes (duplicate patterns, parameter names that conflict, and a GET endpoint
that shadows a view) when it loads the build, which `vorma build` runs as its
last step.
*/

export type ApplicationView = {
	/** The authored pattern, as the loader or the view wrote it. */
	readonly pattern: string;
	readonly compiledPattern: CompiledNestedPattern;
	readonly loader: InspectedViewLoader | undefined;
	readonly clientView: InspectedClientView | undefined;
	/** The input types of the loaders from the chain root to this view, as type text, in chain order; the view's search combines them. */
	readonly chainLoaderInputs: ReadonlyArray<string>;
	/** The authored patterns of the views nested directly in this one; this view's child error component renders their failures. */
	readonly childPatterns: ReadonlyArray<string>;
};

export type ApplicationModel = {
	readonly viewLoaders: ReadonlyArray<InspectedViewLoader>;
	readonly endpoints: ReadonlyArray<InspectedEndpoint>;
	readonly clientViews: ReadonlyArray<InspectedClientView>;
	/** The view modules in stable order, for generated type imports. */
	readonly clientViewModules: ReadonlyArray<string>;
	readonly views: ReadonlyArray<ApplicationView>;
};

/** A problem that stops the build from generating types; the message names every declaration involved. */
export type ApplicationModelIssue = {
	readonly msg: string;
};

type UnlinkedView = Omit<
	ApplicationView,
	"chainLoaderInputs" | "childPatterns"
>;

/** Joins the inspected declarations and views into one model. */
export function compileApplicationModel(
	inspection: InspectedProjects,
): Result.Result<ApplicationModel, ReadonlyArray<ApplicationModelIssue>> {
	const issues: Array<ApplicationModelIssue> = [];
	const clientViews = [...inspection.clientViews].sort(
		Order.mapInput(Order.String, (view) => view.filePath),
	);
	const viewLoaders = inspection.serverDeclarations.filter(
		(declaration): declaration is InspectedViewLoader => {
			return declaration.kind === "ViewLoader";
		},
	);
	const endpoints = inspection.serverDeclarations.filter(
		(declaration): declaration is InspectedEndpoint => {
			return declaration.kind === "Endpoint";
		},
	);

	const viewsByPattern = new Map<string, UnlinkedView>();
	for (const loader of viewLoaders) {
		const compiled = compileViewPattern(loader.pattern);
		if (Result.isFailure(compiled)) {
			issues.push({
				msg: `${loader.location}: ${formatRoutePatternError(compiled.failure)}`,
			});
			continue;
		}
		const key = compiled.success.pattern;
		if (viewsByPattern.has(key)) {
			// The first loader represents its pattern; the server reports the duplicate.
			continue;
		}
		viewsByPattern.set(key, {
			pattern: loader.pattern,
			compiledPattern: compiled.success,
			loader,
			clientView: undefined,
		});
	}
	const compiledClientViews: Array<{
		readonly view: InspectedClientView;
		readonly compiledPattern: CompiledNestedPattern;
	}> = [];
	for (const view of clientViews) {
		const compiled = compileViewPattern(view.pattern);
		if (Result.isFailure(compiled)) {
			issues.push({
				msg: `${view.location}: ${formatRoutePatternError(compiled.failure)}`,
			});
			continue;
		}
		compiledClientViews.push({ view, compiledPattern: compiled.success });
	}
	// Two patterns at one matching position match the same paths even when
	// their parameter names differ; the first view in file order keeps it.
	const clientViewMatcher = createNestedMatcher(
		compiledClientViews.map((entry) => entry.compiledPattern),
	);
	const duplicateIndexes = new Set<number>();
	if (clientViewMatcher.kind === "error") {
		for (const duplicate of clientViewMatcher.error.duplicates) {
			duplicateIndexes.add(duplicate.laterIndex);
			issues.push({
				msg: `${compiledClientViews[duplicate.laterIndex]!.view.location}: the view at ${compiledClientViews[duplicate.earlierIndex]!.view.location} has the same route`,
			});
		}
	}
	const uniqueClientViews = compiledClientViews.filter((_entry, index) => {
		return !duplicateIndexes.has(index);
	});
	for (const { view, compiledPattern } of uniqueClientViews) {
		const key = compiledPattern.pattern;
		const existing = viewsByPattern.get(key);
		viewsByPattern.set(key, {
			pattern: existing === undefined ? view.pattern : existing.pattern,
			compiledPattern,
			loader: existing === undefined ? undefined : existing.loader,
			clientView: view,
		});
	}
	const orderedViews = [...viewsByPattern.values()].sort(
		Order.mapInput(Order.String, (view) => view.pattern),
	);
	for (const declaration of inspection.typeDeclarations.declarations) {
		if (testGeneratedClientIdentifier(declaration.name)) {
			issues.push({
				msg: `The server's TypeScript declaration ${declaration.name} has a name that the generated client types file uses for its own declarations; give the type another name`,
			});
		}
	}
	if (issues.length > 0) {
		return Result.fail(issues);
	}

	const chains = createNestedRouteChains({ nodes: orderedViews });
	// A chain ends with its target, so the view before it is the parent.
	const childPatterns = new Map<string, Array<string>>();
	for (const { target, chain } of chains) {
		const parent = chain.at(-1) === target ? chain.at(-2) : undefined;
		if (parent !== undefined) {
			const siblings = childPatterns.get(parent.pattern);
			if (siblings === undefined) {
				childPatterns.set(parent.pattern, [target.pattern]);
			} else {
				siblings.push(target.pattern);
			}
		}
	}
	const linkedViews = chains.map(({ target, chain }): ApplicationView => {
		return {
			...target,
			childPatterns: childPatterns.get(target.pattern) ?? [],
			chainLoaderInputs: chain.flatMap((node) => {
				return node.loader === undefined ||
					node.loader.input === undefined
					? []
					: [node.loader.input];
			}),
		};
	});
	return Result.succeed({
		viewLoaders,
		endpoints,
		clientViews: uniqueClientViews.map((entry) => entry.view),
		clientViewModules: [
			...new Set(clientViews.map((view) => view.filePath)),
		],
		views: linkedViews,
	});
}
