import { createRequire } from "node:module";
import { parse } from "@babel/parser";
import type traverseModule from "@babel/traverse";
import type { Binding, NodePath } from "@babel/traverse";
import { Result } from "effect";
import { normalizePath } from "vite";

// Babel's CommonJS entry exports the traversal function as `default`.
const traverse = createRequire(import.meta.url)("@babel/traverse")
	.default as typeof traverseModule;

type ResolvedImport = {
	readonly id: string;
	readonly isClientEntry: boolean;
};

type AssetValue =
	| { readonly kind: "assets" }
	| { readonly kind: "namespace"; readonly source: ResolvedImport };

type Module = {
	readonly program: NodePath;
	readonly candidates: ReadonlyArray<NodePath>;
	readonly imports: ReadonlyMap<string, ResolvedImport>;
};

/** Tracks the exported asset binding through ESM imports and constant aliases. */
export function createStaticPublicAssetsReadGraph() {
	const modules = new Map<string, Module>();

	function resolveExport(input: {
		readonly source: ResolvedImport;
		readonly name: string;
		readonly visited: ReadonlySet<unknown>;
	}): AssetValue | undefined {
		const { source, name } = input;
		if (source.isClientEntry && name === "staticPublicAssets") {
			return { kind: "assets" };
		}
		const key = `${source.id}\0${name}`;
		if (input.visited.has(key)) {
			return undefined;
		}
		const visited = new Set(input.visited).add(key);
		const module = modules.get(source.id);
		if (module === undefined) {
			return undefined;
		}
		for (const statement of module.program.get("body") as Array<NodePath>) {
			if (statement.isExportDefaultDeclaration() && name === "default") {
				return resolveExpression({
					module,
					path: statement.get("declaration"),
					visited,
				});
			}
			if (statement.isExportAllDeclaration()) {
				const imported = module.imports.get(
					statement.node.source.value,
				);
				if (imported !== undefined) {
					const found = resolveExport({
						source: imported,
						name,
						visited,
					});
					if (found !== undefined) {
						return found;
					}
				}
			}
			if (!statement.isExportNamedDeclaration()) {
				continue;
			}
			const source = statement.node.source;
			for (const specifier of statement.get("specifiers")) {
				if (readName(specifier.node.exported) !== name) {
					continue;
				}
				const imported =
					source == null
						? undefined
						: module.imports.get(source.value);
				if (specifier.isExportNamespaceSpecifier()) {
					return imported === undefined
						? undefined
						: { kind: "namespace", source: imported };
				}
				if (!specifier.isExportSpecifier()) {
					continue;
				}
				return imported === undefined
					? resolveExpression({
							module,
							path: specifier.get("local"),
							visited,
						})
					: resolveExport({
							source: imported,
							name: readName(specifier.node.local),
							visited,
						});
			}
			const declaration = statement.get("declaration");
			if (declaration.isVariableDeclaration()) {
				const binding = statement.scope.getBinding(name);
				if (
					binding !== undefined &&
					binding.path.parentPath === declaration
				) {
					return resolveBinding({ module, binding, visited });
				}
			}
		}
		return undefined;
	}

	function resolveBinding(input: {
		readonly module: Module;
		readonly binding: Binding;
		readonly visited: ReadonlySet<unknown>;
	}): AssetValue | undefined {
		const { module, binding, visited } = input;
		const path = binding.path;
		if (
			path.isImportSpecifier() ||
			path.isImportDefaultSpecifier() ||
			path.isImportNamespaceSpecifier()
		) {
			const declaration = path.parentPath;
			if (
				!declaration.isImportDeclaration() ||
				declaration.node.importKind === "type"
			) {
				return undefined;
			}
			const source = module.imports.get(declaration.node.source.value);
			if (source === undefined) {
				return undefined;
			}
			if (path.isImportNamespaceSpecifier()) {
				return { kind: "namespace", source };
			}
			if (path.isImportSpecifier() && path.node.importKind === "type") {
				return undefined;
			}
			return resolveExport({
				source,
				name: path.isImportDefaultSpecifier()
					? "default"
					: readName(path.node.imported),
				visited,
			});
		}
		if (
			!binding.constant ||
			binding.kind !== "const" ||
			!path.isVariableDeclarator()
		) {
			return undefined;
		}
		const identifierPath = findBoundIdentifierPath({
			root: path.get("id") as NodePath,
			identifier: binding.identifier,
		});
		return identifierPath === undefined
			? undefined
			: resolveTargetValue({
					module,
					path: identifierPath,
					visited,
				});
	}

	function resolveTargetValue(input: {
		readonly module: Module;
		readonly path: NodePath;
		readonly visited: ReadonlySet<unknown>;
	}): AssetValue | undefined {
		const { module, visited } = input;
		let target = input.path;
		while (
			target.parentPath?.isAssignmentPattern() &&
			target.parentPath.get("left") === target
		) {
			target = target.parentPath;
		}
		const parent = target.parentPath;
		if (parent?.isVariableDeclarator() && parent.get("id") === target) {
			return resolveExpression({
				module,
				path: parent.get("init") as NodePath,
				visited,
			});
		}
		if (
			parent?.isAssignmentExpression({ operator: "=" }) &&
			parent.get("left") === target
		) {
			return resolveExpression({
				module,
				path: parent.get("right") as NodePath,
				visited,
			});
		}
		if (
			parent?.isObjectProperty() &&
			parent.get("value") === target &&
			parent.parentPath.isObjectPattern()
		) {
			const container = resolveTargetValue({
				module,
				path: parent.parentPath,
				visited,
			});
			const name = readPropertyName(parent);
			return container?.kind === "namespace" && name !== undefined
				? resolveExport({ source: container.source, name, visited })
				: undefined;
		}
		return undefined;
	}

	function resolveExpression(input: {
		readonly module: Module;
		readonly path: NodePath;
		readonly visited: ReadonlySet<unknown>;
	}): AssetValue | undefined {
		const { module, path } = input;
		if (path.node == null) {
			return undefined;
		}
		if (input.visited.has(path.node)) {
			return undefined;
		}
		const visited = new Set(input.visited).add(path.node);
		if (path.isIdentifier()) {
			const binding = path.scope.getBinding(path.node.name);
			return binding === undefined
				? undefined
				: resolveBinding({ module, binding, visited });
		}
		if (path.isMemberExpression() || path.isOptionalMemberExpression()) {
			const object = resolveExpression({
				module,
				path: path.get("object") as NodePath,
				visited,
			});
			const name = readPropertyName(path);
			return object?.kind !== "namespace" || name === undefined
				? undefined
				: resolveExport({ source: object.source, name, visited });
		}
		if (path.isAwaitExpression()) {
			const argument = path.get("argument");
			if (
				argument.isCallExpression() &&
				argument.get("callee").isImport()
			) {
				const [specifier] = argument.node.arguments;
				const source =
					specifier?.type === "StringLiteral"
						? module.imports.get(specifier.value)
						: undefined;
				return source === undefined
					? undefined
					: { kind: "namespace", source };
			}
		}
		if (
			path.isTSAsExpression() ||
			path.isTSSatisfiesExpression() ||
			path.isTSNonNullExpression() ||
			path.isTSTypeAssertion() ||
			path.isParenthesizedExpression()
		) {
			return resolveExpression({
				module,
				path: path.get("expression") as NodePath,
				visited,
			});
		}
		return undefined;
	}

	return {
		testRecordedModule(id: string): boolean {
			return modules.has(id);
		},
		deleteModule(idOrFilePath: string): ReadonlySet<string> {
			const targetPath = normalizeModuleFilePath(idOrFilePath);
			const evicted = new Set<string>();
			const queue: Array<string> = [];
			for (const existingId of modules.keys()) {
				if (
					existingId === idOrFilePath ||
					normalizeModuleFilePath(existingId) === targetPath
				) {
					modules.delete(existingId);
					evicted.add(existingId);
					queue.push(existingId);
				}
			}
			for (
				let currentId = queue.pop();
				currentId !== undefined;
				currentId = queue.pop()
			) {
				const currentPath = normalizeModuleFilePath(currentId);
				for (const [candidateId, candidateModule] of modules) {
					let importsCurrent = false;
					for (const resolved of candidateModule.imports.values()) {
						if (
							resolved.id === currentId ||
							normalizeModuleFilePath(resolved.id) === currentPath
						) {
							importsCurrent = true;
							break;
						}
					}
					if (importsCurrent) {
						modules.delete(candidateId);
						evicted.add(candidateId);
						queue.push(candidateId);
					}
				}
			}
			return evicted;
		},
		async recordModule(input: {
			readonly id: string;
			readonly code: string;
			readonly language: "js" | "jsx" | "ts" | "tsx";
			readonly resolveImportFn: (
				source: string,
			) => Promise<ResolvedImport | undefined>;
		}): Promise<void> {
			const parsed = Result.try(() => {
				return parse(input.code, {
					sourceType: "module",
					plugins: [
						...(input.language === "ts" || input.language === "tsx"
							? ["typescript" as const]
							: []),
						...(input.language === "jsx" || input.language === "tsx"
							? ["jsx" as const]
							: []),
					],
				});
			});
			if (Result.isFailure(parsed)) {
				return;
			}
			const program = parsed.success;
			const sources = new Set<string>();
			const candidates: Array<NodePath> = [];
			let root: NodePath | undefined;
			traverse(program, {
				Program(path) {
					root = path;
				},
				ImportDeclaration(path) {
					sources.add(path.node.source.value);
				},
				ExportNamedDeclaration(path) {
					if (path.node.source != null) {
						sources.add(path.node.source.value);
					}
				},
				ExportAllDeclaration(path) {
					sources.add(path.node.source.value);
				},
				CallExpression(path) {
					const [source] = path.node.arguments;
					if (
						path.get("callee").isImport() &&
						source?.type === "StringLiteral"
					) {
						sources.add(source.value);
					}
				},
				ReferencedIdentifier(path) {
					candidates.push(path);
				},
				"MemberExpression|OptionalMemberExpression"(path) {
					candidates.push(path);
				},
				"ObjectPattern|ArrayPattern"(path) {
					candidates.push(path);
				},
			});
			const imports = new Map<string, ResolvedImport>();
			await Promise.all(
				[...sources].map(async (name) => {
					const source = await input.resolveImportFn(name);
					if (source !== undefined) {
						imports.set(name, source);
					}
				}),
			);
			modules.set(input.id, { program: root!, candidates, imports });
		},
		findRead(
			id: string,
		): { readonly kind: "read"; readonly offset: number } | undefined {
			const module = modules.get(id);
			if (module === undefined) {
				return undefined;
			}
			for (const path of module.candidates.toSorted(
				(a, b) => (a.node.start ?? 0) - (b.node.start ?? 0),
			)) {
				if (path.findParent((parent) => parent.isTSType())) {
					continue;
				}
				if (path.isObjectPattern() || path.isArrayPattern()) {
					const targetValue = resolveTargetValue({
						module,
						path,
						visited: new Set(),
					});
					if (targetValue?.kind === "assets") {
						return {
							kind: "read",
							offset: path.node.start ?? 0,
						};
					}
					if (
						path.isObjectPattern() &&
						targetValue?.kind === "namespace"
					) {
						for (const property of path.get("properties")) {
							if (!property.isObjectProperty()) {
								continue;
							}
							const name = readPropertyName(property);
							if (
								name === undefined ||
								resolveExport({
									source: targetValue.source,
									name,
									visited: new Set(),
								})?.kind !== "assets"
							) {
								continue;
							}
							let boundTarget = property.get("value") as NodePath;
							while (boundTarget.isAssignmentPattern()) {
								boundTarget = boundTarget.get(
									"left",
								) as NodePath;
							}
							if (
								boundTarget.isObjectPattern() ||
								boundTarget.isArrayPattern()
							) {
								continue;
							}
							if (boundTarget.isIdentifier()) {
								const binding = boundTarget.scope.getBinding(
									boundTarget.node.name,
								);
								if (
									binding !== undefined &&
									binding.identifier === boundTarget.node &&
									binding.constant &&
									binding.kind === "const" &&
									binding.path.isVariableDeclarator()
								) {
									continue;
								}
							}
							return {
								kind: "read",
								offset:
									property.node.key.start ??
									property.node.start ??
									path.node.start ??
									0,
							};
						}
					}
					continue;
				}
				if (
					resolveExpression({ module, path, visited: new Set() })
						?.kind !== "assets"
				) {
					continue;
				}
				let expression = path;
				while (
					expression.parentPath?.isTSAsExpression() ||
					expression.parentPath?.isTSSatisfiesExpression() ||
					expression.parentPath?.isTSNonNullExpression() ||
					expression.parentPath?.isParenthesizedExpression()
				) {
					expression = expression.parentPath;
				}
				const parent = expression.parentPath;
				if (
					parent?.isExportSpecifier() ||
					parent?.isExportDefaultDeclaration()
				) {
					continue;
				}
				if (
					parent?.isVariableDeclarator() &&
					parent.get("id").isIdentifier() &&
					parent.get("init") === expression &&
					parent.parentPath.isVariableDeclaration({ kind: "const" })
				) {
					continue;
				}
				return {
					kind: "read",
					offset:
						(path.isMemberExpression() ||
						path.isOptionalMemberExpression()
							? path.node.property.start
							: path.node.start) ?? 0,
				};
			}
			return undefined;
		},
	};
}

function normalizeModuleFilePath(id: string): string {
	const queryIndex = id.indexOf("?");
	const filePath = queryIndex === -1 ? id : id.slice(0, queryIndex);
	return normalizePath(filePath);
}

function findBoundIdentifierPath(input: {
	readonly root: NodePath;
	readonly identifier: unknown;
}): NodePath | undefined {
	if (input.root.node === input.identifier) {
		return input.root;
	}
	let found: NodePath | undefined;
	input.root.traverse({
		Identifier(candidate) {
			if (candidate.node === input.identifier) {
				found = candidate;
				candidate.stop();
			}
		},
	});
	return found;
}

type NameNode = {
	readonly type: string;
	readonly name?: string;
	readonly value?: unknown;
};
function readName(node: NameNode): string {
	return node.type === "StringLiteral" ? String(node.value) : node.name!;
}
function readPropertyName(path: NodePath): string | undefined {
	if (
		!path.isMemberExpression() &&
		!path.isOptionalMemberExpression() &&
		!path.isObjectProperty()
	) {
		return undefined;
	}
	const property = path.isObjectProperty()
		? path.node.key
		: path.node.property;
	if (property.type === "StringLiteral") {
		return property.value;
	}
	if (!path.node.computed && property.type === "Identifier") {
		return property.name;
	}
	if (
		property.type === "TemplateLiteral" &&
		property.expressions.length === 0
	) {
		return property.quasis[0]?.value.cooked ?? undefined;
	}
	return undefined;
}
