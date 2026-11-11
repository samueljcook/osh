import { isAbsolute, relative } from "node:path";
import { Effect, Order, Result } from "effect";
import {
	isArrayBindingPattern,
	isAsExpression,
	isBindingElement,
	isComputedPropertyName,
	isElementAccessExpression,
	isExportAssignment,
	isExportSpecifier,
	isIdentifier,
	isImportClause,
	isImportSpecifier,
	isNamespaceImport,
	isNoSubstitutionTemplateLiteral,
	isNonNullExpression,
	isNumericLiteral,
	isObjectBindingPattern,
	isParenthesizedExpression,
	isPropertyAccessExpression,
	isSatisfiesExpression,
	isShorthandPropertyAssignment,
	isStringLiteral,
	isTypeAssertion,
	isTypeNode,
	isVariableDeclaration,
	isVariableDeclarationList,
	NodeFlags,
	SyntaxKind,
	type BindingElement,
	type ElementAccessExpression,
	type Expression,
	type Identifier,
	type Node,
	type ObjectBindingPattern,
	type PropertyAccessExpression,
	type ShorthandPropertyAssignment,
	type SourceFile,
	type VariableDeclaration,
} from "typescript/unstable/ast";
import {
	SymbolFlags,
	type Checker,
	type NodeHandle,
	type Project,
	type Symbol as TypeSymbol,
	type Type,
} from "typescript/unstable/async";
import { createFilter, normalizePath } from "vite";
import type { ApplicationSourceLayout } from "../config/source-layout.ts";
import type { HttpMethod } from "./http-method.ts";
import type { TypeDeclarations } from "./type-declarations.ts";
import { runTsOperation, type TsProjectIssue } from "./typescript-projects.ts";

/*
What the build reads about the application without running the client:

- From the Rust server's declaration contract, each view loader's and API
  endpoint's kind, pattern, method, and the TypeScript text of its types.
- From the client project, through the checker: the views of the view files
  that `client.viewFilePattern` matches, each with its pattern; and every use
  of `staticPublicAssets`: a lookup with a string literal key, which the
  client bundle replaces with the hashed URL, and, as an issue, any other value use. Imports, re-exports, and constant
  aliases preserve the binding identity.

Inspection reports every issue that it finds. Each issue's message names its
place: a path with a line and a column, and for a declaration, the app's list,
the pattern, and an endpoint's method.

A view is a definition with a literal `pattern`. `ViewDef` in
`@vorma/client/react` declares the `component` property on every view, optional
for a pass-through view, so the build recognizes a view by its `pattern` and
`component` properties, whether or not the definition supplies a component, and
reads its pattern from the type; it reads a client loader's output type in the generated types file, by
type, from the view module.
*/

export type InspectedViewLoader = {
	readonly kind: "ViewLoader";
	readonly pattern: string;
	/** Where the app lists the loader, as issue messages name it. */
	readonly location: string;
	/** The input type as TypeScript text; absent when the loader takes no input. */
	readonly input: string | undefined;
	/** The output type as TypeScript text; absent when the loader has no output. */
	readonly output: string | undefined;
	readonly errors: ReadonlyArray<InspectedDeclaredError>;
};

/** One declared error of a view loader or an endpoint. */
export type InspectedDeclaredError = {
	/** The declared error type as TypeScript text. */
	readonly typeText: string;
};

/** How an endpoint receives its input. */
export type InputTransportKind = "none" | "search" | "json" | "formData";

export type InspectedEndpoint = {
	readonly kind: "Endpoint";
	readonly operation: "query" | "mutation";
	readonly method: HttpMethod;
	readonly pattern: string;
	/** Where the app lists the endpoint, as issue messages name it. */
	readonly location: string;
	/** The input type as TypeScript text; absent when the endpoint takes no input. For form data it is the form's fields. */
	readonly input: string | undefined;
	readonly inputTransport: InputTransportKind;
	/** The output type as TypeScript text; absent when the endpoint has no output. */
	readonly output: string | undefined;
	readonly errors: ReadonlyArray<InspectedDeclaredError>;
};

export type InspectedServerDeclaration =
	| InspectedViewLoader
	| InspectedEndpoint;

export type InspectedClientView = {
	/** The view file. */
	readonly filePath: string;
	readonly pattern: string;
	/** Whether the file's default export is the view itself or an array that contains it. */
	readonly defaultExportShape: "View" | "ViewArray";
	/** Where the file exports the view, as issue messages name it. */
	readonly location: string;
};

/** A `staticPublicAssets` lookup: the span to replace and the asset key that it names. */
export type StaticPublicAssetLookup = {
	/** Where the call is, as issue messages name it. */
	readonly location: string;
	readonly filePath: string;
	readonly start: number;
	readonly end: number;
	readonly key: string;
};

export type InspectedProjects = {
	/** Every view loader and endpoint of the application, in the order of their contract identities. */
	readonly serverDeclarations: ReadonlyArray<InspectedServerDeclaration>;
	readonly clientViews: ReadonlyArray<InspectedClientView>;
	readonly staticPublicAssetLookups: ReadonlyArray<StaticPublicAssetLookup>;
	/** The text of every client file that has a static public asset lookup, as the checker read it. */
	readonly clientSourceTexts: ReadonlyMap<string, string>;
	/** Every client source file that the inspection read, as Vite's `normalizePath` spells it. */
	readonly inspectedClientFiles: ReadonlySet<string>;
	/** The named type declarations that the declarations' type texts reference. */
	readonly typeDeclarations: TypeDeclarations;
	/** The server binary's hash of its declarations, which the build directory records. */
	readonly declarationsHash: string;
};

export type InspectionIssue = {
	readonly msg: string;
};

export type InspectionIssues = {
	readonly _tag: "InspectionIssues";
	readonly issues: ReadonlyArray<InspectionIssue>;
};

/** A server's declarations and the named type declarations that their type texts reference. */
export type ServerInspection = Pick<
	InspectedProjects,
	"serverDeclarations" | "typeDeclarations" | "declarationsHash"
>;

/** What the client project contributes to an inspection. */
export type ClientInspection = Omit<InspectedProjects, keyof ServerInspection>;

/** Reads the client project's views and every lookup of a static public asset. */
export function inspectClientProject(input: {
	readonly client: Project;
	readonly layout: ApplicationSourceLayout;
}): Effect.Effect<ClientInspection, InspectionIssues | TsProjectIssue> {
	return Effect.gen(function* () {
		const { layout } = input;
		const { configDir } = layout;
		const clientFiles = yield* runTsOperation(() => {
			return listOwnSourceFiles({
				project: input.client,
				excludedFiles: new Set([
					normalizePath(layout.clientTypesFilePath),
				]),
			});
		});
		const client = yield* runTsOperation(() => {
			return inspectClientFiles({
				checker: input.client.checker,
				files: clientFiles,
				viewFilePattern: layout.viewFilePattern,
				configDir,
				clientTsConfigFilePath: layout.clientTsConfigFilePath,
			});
		});
		if (Result.isFailure(client)) {
			return yield* Effect.fail({
				_tag: "InspectionIssues",
				issues: client.failure,
			} satisfies InspectionIssues);
		}
		return {
			clientViews: client.success.views,
			staticPublicAssetLookups: client.success.staticPublicAssetLookups,
			clientSourceTexts: client.success.sourceTexts,
			inspectedClientFiles: new Set(
				clientFiles.map((file) => normalizePath(file.fileName)),
			),
		};
	});
}

/** A file position as issue messages name it: the path relative to the configuration directory, the line, and the column. */
function formatSourcePosition(input: {
	readonly configDir: string;
	readonly sourceFile: SourceFile;
	readonly node: Node;
}): string {
	const { line, character } = input.sourceFile.getLineAndCharacterOfPosition(
		input.node.getStart(input.sourceFile),
	);
	return `${relative(input.configDir, input.sourceFile.fileName)}:${line + 1}:${character + 1}`;
}

/**
 * The program's own modules: every file of the program that is not a
 * declaration file, not a default library, not a file that the build
 * generates, and not in `node_modules`. TypeScript names a module that it
 * resolves through `node_modules` by its real path, so a workspace package
 * that a link in `node_modules` names is one of these modules, although
 * TypeScript reports it as an external library.
 */
async function listOwnSourceFiles(input: {
	readonly project: Project;
	readonly excludedFiles: ReadonlySet<string>;
}): Promise<ReadonlyArray<SourceFile>> {
	const { program } = input.project;
	const files: Array<SourceFile> = [];
	for (const fileName of await program.getSourceFileNames()) {
		if (
			fileName.includes("/node_modules/") ||
			input.excludedFiles.has(normalizePath(fileName))
		) {
			continue;
		}
		// Compiler metadata avoids transferring dependency syntax trees over IPC.
		const metadata = await program.getSourceFileMetadata(fileName);
		if (metadata?.isDefaultLibrary) {
			continue;
		}
		const sourceFile = await program.getSourceFile(fileName);
		if (sourceFile === undefined || sourceFile.isDeclarationFile) {
			continue;
		}
		files.push(sourceFile);
	}
	return files.sort(Order.mapInput(Order.String, (file) => file.fileName));
}

type ModuleExport = {
	readonly exportName: string;
	/** The export's own symbol, followed through any alias to the declaring symbol. */
	readonly symbol: TypeSymbol;
	readonly type: Type;
	/** The export's declaration in this file, such as `export default`; absent for a re-export from another file. */
	readonly declaration: NodeHandle | undefined;
};

/** Export bindings retain their public names while aliases resolve to one declaration identity. */
async function listModuleExports(input: {
	readonly checker: Checker;
	readonly sourceFile: SourceFile;
}): Promise<ReadonlyArray<ModuleExport>> {
	const { checker, sourceFile } = input;
	const moduleSymbol = await checker.getSymbolAtLocation(sourceFile);
	if (moduleSymbol === undefined) {
		return [];
	}
	const exports: Array<ModuleExport> = [];
	for (const exported of await checker.getExportsOfModule(moduleSymbol)) {
		const symbol =
			(exported.flags & SymbolFlags.Alias) !== 0
				? await checker.getAliasedSymbol(exported)
				: exported;
		if (await checker.isUnknownSymbol(symbol)) {
			continue;
		}
		const type = await checker.getTypeOfSymbol(symbol);
		if (type === undefined) {
			continue;
		}
		const [declaration] = exported.declarations;
		exports.push({
			exportName: exported.name,
			symbol,
			type,
			declaration:
				declaration?.path === sourceFile.path ? declaration : undefined,
		});
	}
	return exports;
}

async function readPropertyType(input: {
	readonly checker: Checker;
	readonly type: Type;
	readonly name: string;
}): Promise<Type | undefined> {
	const property = await input.checker.getPropertyOfType(
		input.type,
		input.name,
	);
	return property === undefined
		? undefined
		: await input.checker.getTypeOfSymbol(property);
}

async function readLiteralProperty(input: {
	readonly checker: Checker;
	readonly type: Type;
	readonly name: string;
}): Promise<Result.Result<string, string>> {
	const propertyType = await readPropertyType(input);
	if (propertyType === undefined) {
		return Result.fail(`the declaration has no ${input.name} property`);
	}
	if (!propertyType.isStringLiteralType()) {
		return Result.fail(
			`${input.name} must have a literal type; its type is ${await input.checker.typeToString(propertyType)}`,
		);
	}
	return Result.succeed(propertyType.value);
}

/** A union's members, the type itself when it is not a union, and nothing for `never`. */
async function listUnionMembers(type: Type): Promise<ReadonlyArray<Type>> {
	if (type.isIntrinsicType() && type.intrinsicName === "never") {
		return [];
	}
	return type.isUnionType() ? ((await type.getTypes()) ?? []) : [type];
}

/** Where an issue is reported: a file position, and for a declaration, its list, method, and pattern. */
type IssueLocation = {
	readonly text: string;
};

/** An issue at a location; its message is complete on its own. */
function createIssue(location: IssueLocation, msg: string): InspectionIssue {
	return { msg: `${location.text}: ${msg}` };
}

type InspectedClientProject = {
	readonly views: ReadonlyArray<InspectedClientView>;
	readonly staticPublicAssetLookups: ReadonlyArray<StaticPublicAssetLookup>;
	readonly sourceTexts: ReadonlyMap<string, string>;
};

async function inspectClientFiles(input: {
	readonly checker: Checker;
	readonly files: ReadonlyArray<SourceFile>;
	readonly viewFilePattern: string;
	readonly configDir: string;
	readonly clientTsConfigFilePath: string;
}): Promise<
	Result.Result<InspectedClientProject, ReadonlyArray<InspectionIssue>>
> {
	const { checker, configDir } = input;
	const views = await inspectClientViews(input);
	const issues: Array<InspectionIssue> = [...views.issues];
	const staticPublicAssetLookups: Array<StaticPublicAssetLookup> = [];
	const sourceTexts = new Map<string, string>();
	// Every file's lookups are read concurrently and examined in file order.
	const fileCollections = await Promise.all(
		input.files.map((sourceFile) => {
			return collectStaticPublicAssetLookups({
				checker,
				sourceFile,
				configDir,
			});
		}),
	);
	for (const [index, sourceFile] of input.files.entries()) {
		const collection = fileCollections[index];
		if (collection === undefined) {
			continue;
		}
		issues.push(...collection.issues);
		if (collection.lookups.length > 0) {
			staticPublicAssetLookups.push(...collection.lookups);
			sourceTexts.set(sourceFile.fileName, sourceFile.text);
		}
	}
	if (issues.length > 0) {
		return Result.fail(issues);
	}
	return Result.succeed({
		views: views.views,
		staticPublicAssetLookups,
		sourceTexts,
	});
}

/** The views that one export contains: none, a view, the views of an array, or an array that mixes views with other values. */
type ExportedViews =
	| { readonly _tag: "None" }
	| {
			readonly _tag: "Views";
			readonly defaultExportShape: "View" | "ViewArray";
			readonly types: ReadonlyArray<Type>;
	  }
	| { readonly _tag: "Mixed" };

async function testViewType(input: {
	readonly checker: Checker;
	readonly type: Type;
}): Promise<boolean> {
	const [pattern, component] = await Promise.all([
		input.checker.getPropertyOfType(input.type, "pattern"),
		input.checker.getPropertyOfType(input.type, "component"),
	]);
	return pattern !== undefined && component !== undefined;
}

async function readExportedViews(input: {
	readonly checker: Checker;
	readonly type: Type;
}): Promise<ExportedViews> {
	const { checker, type } = input;
	if (await testViewType(input)) {
		return { _tag: "Views", defaultExportShape: "View", types: [type] };
	}
	const [isArray, isTuple] = await Promise.all([
		checker.isArrayType(type),
		checker.isTupleType(type),
	]);
	if ((!isArray && !isTuple) || !type.isTypeReference()) {
		return { _tag: "None" };
	}
	const elements = (
		await Promise.all(
			(await checker.getTypeArguments(type)).map(listUnionMembers),
		)
	).flat();
	const matches = await Promise.all(
		elements.map((element) => testViewType({ checker, type: element })),
	);
	if (!matches.includes(true)) {
		return { _tag: "None" };
	}
	return matches.includes(false)
		? { _tag: "Mixed" }
		: { _tag: "Views", defaultExportShape: "ViewArray", types: elements };
}

/**
 * The views of the client project's files that `client.viewFilePattern` matches,
 * a search scope. A matched file is a view file when its default export is a
 * view or an array of views, and other matched files are ignored. A view
 * exported under another name, or a default-exported array that mixes views
 * with other values, is an issue. Any number of views is valid, including
 * none, but a pattern that matches no file of the project is an issue.
 */
async function inspectClientViews(input: {
	readonly checker: Checker;
	readonly files: ReadonlyArray<SourceFile>;
	readonly viewFilePattern: string;
	readonly configDir: string;
	readonly clientTsConfigFilePath: string;
}): Promise<{
	readonly views: ReadonlyArray<InspectedClientView>;
	readonly issues: ReadonlyArray<InspectionIssue>;
}> {
	const { checker, configDir } = input;
	// The filter resolves a relative pattern against the escaped configuration
	// directory, so a glob character in the project's path matches only
	// itself. It leaves a pattern that starts with `**` unresolved, so the
	// pattern starts with `./` when it is relative.
	const testViewFile = createFilter(
		isAbsolute(input.viewFilePattern)
			? input.viewFilePattern
			: `./${input.viewFilePattern}`,
		undefined,
		{ resolve: configDir },
	);
	const viewFiles = input.files.filter((sourceFile) => {
		return testViewFile(normalizePath(sourceFile.fileName));
	});
	const views: Array<InspectedClientView> = [];
	const issues: Array<InspectionIssue> =
		viewFiles.length === 0
			? [
					{
						msg: `client.viewFilePattern ${JSON.stringify(input.viewFilePattern)} matches no source file of the client project ${relative(configDir, input.clientTsConfigFilePath)}. Correct the pattern.`,
					},
				]
			: [];
	for (const sourceFile of viewFiles) {
		for (const moduleExport of await listModuleExports({
			checker,
			sourceFile,
		})) {
			const exported = await readExportedViews({
				checker,
				type: moduleExport.type,
			});
			if (exported._tag === "None") {
				continue;
			}
			const declaration = await moduleExport.declaration?.resolve();
			const location: IssueLocation = {
				text: `${
					declaration === undefined
						? relative(configDir, sourceFile.fileName)
						: formatSourcePosition({
								configDir,
								sourceFile,
								node: declaration,
							})
				} (export ${moduleExport.exportName})`,
			};
			if (moduleExport.exportName !== "default") {
				issues.push(
					createIssue(
						location,
						"a view file exports its views only as its default export, which is a view or an array of views",
					),
				);
				continue;
			}
			if (exported._tag === "Mixed") {
				issues.push(
					createIssue(
						location,
						"a view file's default-exported array must contain only views",
					),
				);
				continue;
			}
			for (const type of exported.types) {
				const pattern = await readLiteralProperty({
					checker,
					type,
					name: "pattern",
				});
				if (Result.isFailure(pattern)) {
					issues.push(createIssue(location, pattern.failure));
					continue;
				}
				views.push({
					filePath: sourceFile.fileName,
					pattern: pattern.success,
					defaultExportShape: exported.defaultExportShape,
					location: `${location.text} ${JSON.stringify(pattern.success)}`,
				});
			}
		}
	}
	return { views, issues };
}

/**
 * Whether a symbol is the `staticPublicAssets` that `@vorma/client` declares,
 * however the module reached it: through imports and re-exports, and through
 * a `const` that names it, such as `export const assets = staticPublicAssets`.
 */
async function testStaticPublicAssetsSymbol(input: {
	readonly checker: Checker;
	readonly symbol: TypeSymbol;
}): Promise<boolean> {
	let symbol = input.symbol;
	const visited = new Set<number>();
	while (!visited.has(symbol.id)) {
		visited.add(symbol.id);
		if ((symbol.flags & SymbolFlags.Alias) !== 0) {
			symbol = await input.checker.getAliasedSymbol(symbol);
			continue;
		}
		if (
			symbol.name === "staticPublicAssets" &&
			(await symbol.getJsDocTags(input.checker)).some(
				(tag) => tag.name === "vormaStaticPublicAsset",
			)
		) {
			return true;
		}
		const aliased = await findStaticPublicAssetsConstant({
			checker: input.checker,
			symbol,
		});
		if (aliased === undefined) {
			return false;
		}
		symbol = aliased;
	}
	return false;
}

/**
 * The symbol that a `const` names when its whole initializer is a use of
 * another symbol, or when an object destructuring `const` binds a static
 * property of another symbol.
 */
async function findStaticPublicAssetsConstant(input: {
	readonly checker: Checker;
	readonly symbol: TypeSymbol;
}): Promise<TypeSymbol | undefined> {
	const declaration = input.symbol.valueDeclaration;
	if (
		declaration?.kind !== SyntaxKind.VariableDeclaration &&
		declaration?.kind !== SyntaxKind.ExportAssignment &&
		declaration?.kind !== SyntaxKind.BindingElement
	) {
		return undefined;
	}
	const node = await declaration.resolve();
	if (
		node !== undefined &&
		testObjectBindingElement(node) &&
		testConstantBindingElement(node)
	) {
		const propertySymbol = (
			await resolveUseSymbols({
				checker: input.checker,
				uses: [node],
			})
		)[0];
		if (
			propertySymbol !== undefined &&
			propertySymbol.id !== input.symbol.id
		) {
			return propertySymbol;
		}
		if (node.initializer === undefined) {
			return undefined;
		}
	}
	const initializer =
		node !== undefined && isExportAssignment(node)
			? node.expression
			: node !== undefined && testConstantDeclaration(node)
				? node.initializer
				: node !== undefined &&
					  testObjectBindingElement(node) &&
					  testConstantBindingElement(node)
					? node.initializer
					: undefined;
	if (initializer === undefined) {
		return undefined;
	}
	const use = unwrapExpression(initializer);
	if (
		!isIdentifier(use) &&
		!isPropertyAccessExpression(use) &&
		!isElementAccessExpression(use)
	) {
		return undefined;
	}
	return (
		await resolveUseSymbols({
			checker: input.checker,
			uses: [use],
		})
	)[0];
}

/** A `const` declaration with an initializer. */
function testConstantDeclaration(
	node: Node,
): node is VariableDeclaration & { readonly initializer: Expression } {
	return (
		isVariableDeclaration(node) &&
		node.initializer !== undefined &&
		isVariableDeclarationList(node.parent) &&
		(node.parent.flags & NodeFlags.Const) !== 0
	);
}

type ObjectBindingElement = BindingElement & {
	readonly parent: ObjectBindingPattern;
};

function testObjectBindingElement(node: Node): node is ObjectBindingElement {
	return isBindingElement(node) && isObjectBindingPattern(node.parent);
}

function testConstantBindingElement(node: ObjectBindingElement): boolean {
	let current: Node = node.parent;
	while (
		isBindingElement(current.parent) ||
		isObjectBindingPattern(current.parent) ||
		isArrayBindingPattern(current.parent)
	) {
		current = current.parent;
	}
	return (
		testConstantDeclaration(current.parent) &&
		current.parent.name === current
	);
}

type StaticPublicAssetsUse =
	| Identifier
	| PropertyAccessExpression
	| ElementAccessExpression
	| ObjectBindingElement;

/**
 * Every lookup of `staticPublicAssets` in one file. A use that is not a lookup
 * with a string literal key, or with an identifier as a property name, cannot
 * be replaced by the build. Imports, re-exports, and constant aliases preserve
 * the binding identity. The file's candidate
 * uses are collected in source order and resolved in batched checker
 * requests; a matched use's own sub-expressions are not uses.
 */
async function collectStaticPublicAssetLookups(input: {
	readonly checker: Checker;
	readonly sourceFile: SourceFile;
	readonly configDir: string;
}): Promise<{
	readonly lookups: ReadonlyArray<StaticPublicAssetLookup>;
	readonly issues: ReadonlyArray<InspectionIssue>;
}> {
	const { checker, sourceFile, configDir } = input;
	const uses: Array<StaticPublicAssetsUse> = [];
	function collect(node: Node): void {
		if (isTypeNode(node)) {
			return;
		}
		if (testObjectBindingElement(node)) {
			uses.push(node);
		} else if (
			isIdentifier(node) ||
			isPropertyAccessExpression(node) ||
			isElementAccessExpression(node)
		) {
			const parent = node.parent;
			// Import, export, and bound constant names are not uses; an object
			// binding element itself represents the property read.
			if (
				isImportSpecifier(parent) ||
				isImportClause(parent) ||
				isNamespaceImport(parent) ||
				isExportSpecifier(parent) ||
				(isVariableDeclaration(parent) && parent.name === node) ||
				(isBindingElement(parent) &&
					(parent.name === node || parent.propertyName === node))
			) {
				return;
			}
			uses.push(node);
		}
		node.forEachChild((child) => {
			collect(child);
		});
	}
	collect(sourceFile);
	const symbols = await resolveUseSymbols({ checker, uses });
	const matchesBySymbol = new Map<number, Promise<boolean>>();
	function testUseOfStaticPublicAssets(symbol: TypeSymbol): Promise<boolean> {
		let matches = matchesBySymbol.get(symbol.id);
		if (matches === undefined) {
			matches = testStaticPublicAssetsSymbol({ checker, symbol });
			matchesBySymbol.set(symbol.id, matches);
		}
		return matches;
	}
	function toLocation(node: Node): IssueLocation {
		return {
			text: formatSourcePosition({
				configDir,
				sourceFile,
				node,
			}),
		};
	}
	const lookups: Array<StaticPublicAssetLookup> = [];
	/** Each issue with the position of its node so that the issues follow the source order. */
	const issues: Array<{
		readonly position: number;
		readonly issue: InspectionIssue;
	}> = [];
	function reportIssue(node: Node, msg: string): void {
		issues.push({
			position: node.getStart(sourceFile),
			issue: createIssue(toLocation(node), msg),
		});
	}
	/** The end of the last matched use; the uses before it are its own sub-expressions. */
	let matchedEnd = -1;
	for (const [index, node] of uses.entries()) {
		const symbol = symbols[index];
		if (
			node.pos < matchedEnd ||
			symbol === undefined ||
			!(await testUseOfStaticPublicAssets(symbol))
		) {
			continue;
		}
		matchedEnd = node.end;
		let callee: Node = node;
		while (
			testTransparentExpression(callee.parent) &&
			callee.parent.expression === callee
		) {
			callee = callee.parent;
		}
		// A `const` that names staticPublicAssets defines an alias; its
		// lookups are checked where they occur. A destructuring `const` reads
		// properties at run time, so it is not an alias.
		if (
			testConstantDeclaration(callee.parent) &&
			callee.parent.initializer === callee &&
			isIdentifier(callee.parent.name)
		) {
			continue;
		}
		if (isExportAssignment(callee.parent)) {
			continue;
		}
		const lookup = callee.parent;
		if (
			!(
				isElementAccessExpression(lookup) ||
				isPropertyAccessExpression(lookup)
			) ||
			lookup.expression !== callee
		) {
			reportIssue(
				node,
				"staticPublicAssets must be read directly with a string literal key so the build can replace the lookup",
			);
			continue;
		}
		const key = readLookupKey(lookup);
		if (key === undefined) {
			reportIssue(
				lookup,
				"a staticPublicAssets lookup requires a string literal key, so the build can replace the lookup with the hashed URL",
			);
			continue;
		}
		lookups.push({
			location: toLocation(lookup).text,
			filePath: sourceFile.fileName,
			start: lookup.getStart(sourceFile),
			end: lookup.getEnd(),
			key,
		});
	}
	return {
		lookups,
		issues: issues
			.toSorted((first, second) => first.position - second.position)
			.map((entry) => entry.issue),
	};
}

/** The asset key that a lookup names: a string literal in brackets, or an identifier after a dot. */
function readLookupKey(
	lookup: ElementAccessExpression | PropertyAccessExpression,
): string | undefined {
	if (isPropertyAccessExpression(lookup)) {
		return isIdentifier(lookup.name) ? lookup.name.text : undefined;
	}
	const argument = unwrapExpression(lookup.argumentExpression);
	return isStringLiteral(argument) ||
		isNoSubstitutionTemplateLiteral(argument)
		? argument.text
		: undefined;
}

/**
 * Each use's symbol, index for index: batched checker requests for named uses,
 * element access types, and object destructuring binding elements. The name of
 * a shorthand property, as in `{ staticPublicAssets }`, resolves to the value
 * that it reads, and an object destructuring element, such as
 * `const { staticPublicAssets } = Client` or
 * `const { ["staticPublicAssets"]: assets } = Client`, resolves to the
 * property that it reads from the destructured type.
 */
async function resolveUseSymbols(input: {
	readonly checker: Checker;
	readonly uses: ReadonlyArray<StaticPublicAssetsUse>;
}): Promise<ReadonlyArray<TypeSymbol | undefined>> {
	const { checker, uses } = input;
	const named = uses.filter((node) => {
		return (
			!isElementAccessExpression(node) &&
			!testObjectBindingElement(node) &&
			!testShorthandPropertyName(node)
		);
	});
	const elementAccesses = uses.filter(isElementAccessExpression);
	const bindingElements = uses.filter(testObjectBindingElement);
	const [namedSymbols, elementSymbols, bindingSymbols, shorthandSymbols] =
		await Promise.all([
			named.length === 0 ? [] : checker.getSymbolAtLocation(named),
			getElementAccessSymbols({ checker, nodes: elementAccesses }),
			getObjectBindingElementSymbols({ checker, nodes: bindingElements }),
			Promise.all(
				uses.map(async (node) => {
					if (testShorthandPropertyName(node)) {
						return await checker.getShorthandAssignmentValueSymbol(
							node.parent,
						);
					}
					return undefined;
				}),
			),
		]);
	let namedIndex = 0;
	let elementIndex = 0;
	let bindingIndex = 0;
	return uses.map((node, index) => {
		if (isElementAccessExpression(node)) {
			return elementSymbols[elementIndex++];
		}
		if (testObjectBindingElement(node)) {
			return bindingSymbols[bindingIndex++];
		}
		if (testShorthandPropertyName(node)) {
			return shorthandSymbols[index];
		}
		return namedSymbols[namedIndex++];
	});
}

/** The name of a shorthand property assignment, as in `{ staticPublicAssets }`. */
function testShorthandPropertyName(
	node: Node,
): node is Identifier & { readonly parent: ShorthandPropertyAssignment } {
	return (
		isIdentifier(node) &&
		isShorthandPropertyAssignment(node.parent) &&
		node.parent.name === node
	);
}

/** The static property key that an object destructuring binding element reads, if any. */
function readBindingElementKey(node: BindingElement): string | undefined {
	if (node.dotDotDotToken !== undefined) {
		return undefined;
	}
	const property = node.propertyName;
	if (property === undefined) {
		return node.name !== undefined && isIdentifier(node.name)
			? node.name.text
			: undefined;
	}
	if (
		isIdentifier(property) ||
		isStringLiteral(property) ||
		isNumericLiteral(property)
	) {
		return property.text;
	}
	if (isComputedPropertyName(property)) {
		const expression = unwrapExpression(property.expression);
		return isStringLiteral(expression) ||
			isNoSubstitutionTemplateLiteral(expression) ||
			isNumericLiteral(expression)
			? expression.text
			: undefined;
	}
	return undefined;
}

/** Resolves each object destructuring binding element to the property symbol on the destructured object's type. */
async function getObjectBindingElementSymbols(input: {
	readonly checker: Checker;
	readonly nodes: ReadonlyArray<ObjectBindingElement>;
}): Promise<ReadonlyArray<TypeSymbol | undefined>> {
	const { checker, nodes } = input;
	if (nodes.length === 0) {
		return [];
	}
	const keys = nodes.map(readBindingElementKey);
	const types = await checker.getTypeAtLocation(
		nodes.map((node) => node.parent),
	);
	return Promise.all(
		nodes.map(async (_node, index) => {
			const patternType = types[index];
			const key = keys[index];
			return patternType !== undefined && key !== undefined
				? await checker.getPropertyOfType(patternType, key)
				: undefined;
		}),
	);
}

/** Parentheses and erased TypeScript wrappers preserve the underlying expression. */
function testTransparentExpression(node: Node) {
	return (
		isParenthesizedExpression(node) ||
		isAsExpression(node) ||
		isTypeAssertion(node) ||
		isSatisfiesExpression(node) ||
		isNonNullExpression(node)
	);
}

function unwrapExpression(node: Node): Node {
	while (testTransparentExpression(node)) {
		node = node.expression;
	}
	return node;
}

/** An element access names a property only through a string literal key; its symbol is that property of the object's type. */
async function getElementAccessSymbols(input: {
	readonly checker: Checker;
	readonly nodes: ReadonlyArray<ElementAccessExpression>;
}): Promise<ReadonlyArray<TypeSymbol | undefined>> {
	const { checker, nodes } = input;
	if (nodes.length === 0) {
		return [];
	}
	const types = await checker.getTypeAtLocation(
		nodes.flatMap((node) => [node.expression, node.argumentExpression]),
	);
	return Promise.all(
		nodes.map(async (_node, index) => {
			const objectType = types[index * 2];
			const keyType = types[index * 2 + 1];
			return objectType !== undefined && keyType?.isStringLiteralType()
				? await checker.getPropertyOfType(objectType, keyType.value)
				: undefined;
		}),
	);
}
