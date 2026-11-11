import { GENERATED_TYPES_PROTOCOL } from "@vorma/client/unstable";
import type * as Path from "effect/Path";
import {
	renderAssetKeyDeclaration,
	type StaticPublicAssetProjection,
} from "../assets/static-public-assets.ts";
import type {
	InspectedClientView,
	InspectedProjects,
} from "../inspection/inspect-projects.ts";
import { selectTypeDeclarations } from "../inspection/type-declarations.ts";
import type { ApplicationModel } from "./compile-application.ts";
import {
	GENERATED_SOURCE_HEADER,
	getModuleSpecifier,
	indentRenderedText,
	type GeneratedSource,
} from "./render-source-text.ts";

/*
The client project's generated types: each view's and endpoint's types, as
the TypeScript text that the Rust server's contract carries, with the named
declarations that the texts reference; each view's client loader output,
read from the view module by type; and the static public asset names. The file
augments `VormaGeneratedTypes` of `@vorma/client`, and no application file
imports it. It imports only the view modules, as types, and it carries no
code: the browser receives every value from the server by value, and this file
only names the types of what arrives.

The server's named declarations share the file's scope with the file's own
identifiers. Every own identifier starts with `$`, which no Rust type name
contains, so a server type of any Rust name keeps its name. The augmentation
refers only to own identifiers, because inside a module augmentation the
exports of `@vorma/client` shadow file-level names. A name that a hand-written
TypeScript declaration gives is checked against the own identifiers.
*/

/** An identifier that the generated file declares or imports, besides the server's named declarations. */
export function testGeneratedClientIdentifier(name: string): boolean {
	return /^\$(?:ClientViewModule\d+|ClientLoaderSuccess|Extract|Views|ApiEndpoints|Assets)$/.test(
		name,
	);
}

/** Renders the augmentation of `VormaGeneratedTypes` with the fields `views`, `apiEndpoints`, and `assets`. */
export function renderGeneratedClientTypes(input: {
	readonly model: ApplicationModel;
	readonly inspection: InspectedProjects;
	readonly staticPublicAssets: StaticPublicAssetProjection;
	readonly filePath: string;
	readonly path: Path.Path;
}): GeneratedSource {
	function toSpecifier(filePath: string): string {
		return JSON.stringify(
			getModuleSpecifier({
				path: input.path,
				fromFilePath: input.filePath,
				toFilePath: filePath,
			}),
		);
	}
	const viewModuleIdentifiers = new Map(
		input.model.clientViewModules.map((filePath, index) => {
			return [filePath, `$ClientViewModule${index}`] as const;
		}),
	);
	/** The type of a view: the module's default export, or the member of its default-exported array with the view's pattern. */
	function renderViewType(view: InspectedClientView): string {
		const exported = `(typeof ${viewModuleIdentifiers.get(view.filePath)})["default"]`;
		return view.defaultExportShape === "View"
			? exported
			: `$Extract<${exported}[number], { readonly pattern: ${JSON.stringify(view.pattern)} }>`;
	}
	const viewTypes = input.model.views.map((view) => {
		return renderTypeEntry(view.pattern, {
			input: view.loader?.input ?? "undefined",
			output: view.loader?.output ?? "undefined",
			search:
				view.chainLoaderInputs.length === 0
					? "undefined"
					: view.chainLoaderInputs.join(" &\n\t\t\t"),
			clientLoader:
				view.clientView === undefined
					? "{ readonly success: undefined }"
					: `{ readonly success: $ClientLoaderSuccess<${renderViewType(view.clientView)}> }`,
			errors: renderErrorTypes(view.loader?.errors ?? []),
			childPatterns:
				view.childPatterns.length === 0
					? "never"
					: view.childPatterns
							.map((pattern) => JSON.stringify(pattern))
							.join(" | "),
		});
	});
	const endpointTypes = input.model.endpoints.map((endpoint) => {
		return renderTypeEntry(`${endpoint.method} ${endpoint.pattern}`, {
			kind: JSON.stringify(endpoint.operation),
			// A form endpoint's fields are its `formInput`; every other input is `input`.
			...(endpoint.inputTransport === "formData"
				? { formInput: endpoint.input ?? "undefined" }
				: { input: endpoint.input ?? "undefined" }),
			output: endpoint.output ?? "undefined",
			errors: renderErrorTypes(endpoint.errors),
		});
	});
	const declarations = selectTypeDeclarations({
		typeDeclarations: input.inspection.typeDeclarations,
		// The rendered texts that this file includes.
		typeTexts: [
			...input.model.views.flatMap((view) => {
				return [
					view.loader?.input,
					view.loader?.output,
					...(view.loader?.errors ?? []).map((error) => {
						return error.typeText;
					}),
					...view.chainLoaderInputs,
				];
			}),
			...input.model.endpoints.flatMap((endpoint) => {
				return [
					endpoint.input,
					endpoint.output,
					...endpoint.errors.map((error) => error.typeText),
				];
			}),
		].filter((text) => text !== undefined),
	});
	const hasArrayView = input.model.views.some((view) => {
		return view.clientView?.defaultExportShape === "ViewArray";
	});
	const hasView = input.model.clientViewModules.length > 0;
	return {
		filePath: input.filePath,
		source: [
			GENERATED_SOURCE_HEADER,
			"",
			...input.model.clientViewModules.map((filePath) => {
				return `import type * as ${viewModuleIdentifiers.get(filePath)} from ${toSpecifier(filePath)};`;
			}),
			// The file is a module, so that its `declare module` augments.
			"export {};",
			...(hasArrayView
				? ["", "type $Extract<T, U> = T extends U ? T : never;"]
				: []),
			...(hasView
				? [
						"",
						"/** The output of a view's client loader; `undefined` for a view without one. */",
						"type $ClientLoaderSuccess<View> = View extends {",
						"\treadonly clientLoader?: infer Loader;",
						"}",
						"\t? Loader extends (ctx: never) => infer Output",
						"\t\t? Awaited<Output>",
						"\t\t: undefined",
						"\t: undefined;",
					]
				: []),
			"",
			...(declarations.length === 0 ? [] : [...declarations, ""]),
			"type $Views = {",
			...viewTypes,
			"};",
			"",
			"type $ApiEndpoints = {",
			...endpointTypes,
			"};",
			"",
			renderAssetKeyDeclaration({
				assets: input.staticPublicAssets.assets,
				depth: 0,
				prefix: "type $Assets =",
			}),
			"",
			`declare module ${JSON.stringify(GENERATED_TYPES_PROTOCOL.moduleSpecifier)} {`,
			`\tinterface ${GENERATED_TYPES_PROTOCOL.interfaceName} {`,
			"\t\treadonly views: $Views;",
			"\t\treadonly apiEndpoints: $ApiEndpoints;",
			"\t\treadonly assets: $Assets;",
			"\t}",
			"}",
			"",
		].join("\n"),
	};
}

/** One keyed entry of a contract map, its properties one per line. */
function renderTypeEntry(
	key: string,
	properties: Readonly<Record<string, string>>,
): string {
	return [
		`\treadonly ${JSON.stringify(key)}: {`,
		...Object.entries(properties).map(([name, type]) => {
			return `\t\treadonly ${name}: ${type};`;
		}),
		"\t};",
	].join("\n");
}

/** An operation's declared error payloads, keyed by their declaration positions. */
function renderErrorTypes(
	errors: ReadonlyArray<{ readonly typeText: string }>,
): string {
	if (errors.length === 0) {
		return "{}";
	}
	return [
		"{",
		...errors.map((error, index) => {
			return `\t\t\treadonly ${JSON.stringify(String(index))}: ${indentRenderedText(error.typeText, 1)};`;
		}),
		"\t\t}",
	].join("\n");
}
