import { defineRule, type ESTree } from "@oxlint/plugins";

export const RULE_NAME = "convertible-naming-convention";

const VALUE_MSG_ID = "value_name";
const TYPE_MSG_ID = "type_name";

type NamedIdentifier =
	| ESTree.IdentifierName
	| ESTree.IdentifierReference
	| ESTree.BindingIdentifier
	| ESTree.PrivateIdentifier;

function withoutLeadingUnderscores(name: string): string {
	return name.replace(/^_+/, "");
}

function testConvertibleValueName(name: string): boolean {
	const bare = withoutLeadingUnderscores(name);
	return (
		// A name of only underscores, such as `_`, has no words to case.
		bare === "" ||
		(/^[a-z][a-zA-Z0-9]*$/.test(bare) && !/[A-Z]{2}/.test(bare)) ||
		(/^[A-Z][a-zA-Z0-9]*$/.test(bare) && !/[A-Z]{2}/.test(bare)) ||
		/^[a-z][a-z0-9]*(?:_[a-z0-9]+)*$/.test(bare) ||
		/^[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)*$/.test(bare)
	);
}

function testStrictPascalCase(name: string): boolean {
	return /^[A-Z][a-zA-Z0-9]*$/.test(name) && !/[A-Z]{2}/.test(name);
}

function testTypeParameterName(name: string): boolean {
	return (
		testStrictPascalCase(name) ||
		(name.startsWith("T") && testStrictPascalCase(name.slice(1)))
	);
}

/**
 * Tests whether a node is inside `declare global` or inside a module declaration
 * with a string name. Those declarations augment or describe declarations that
 * another package owns, so the names inside them are not owned here.
 */
function testInsideExternalDeclaration(node: ESTree.Node): boolean {
	const parent = node.parent;
	if (parent === null) {
		return false;
	}
	if (
		parent.type === "TSModuleDeclaration" &&
		(parent.kind === "global" || parent.id.type === "Literal")
	) {
		return true;
	}
	return testInsideExternalDeclaration(parent);
}

type NameKind = "type" | "value";

/**
 * Returns `"type"` when `node` declares `name` as a class, interface, type
 * alias, enum, or type-only import, `"value"` when `node` declares `name` in any
 * other way, and `undefined` when `node` does not declare `name`.
 */
function findDeclaredNameKind(input: {
	readonly node: ESTree.Node;
	readonly name: string;
}): NameKind | undefined {
	const { node, name } = input;
	switch (node.type) {
		case "ExportNamedDeclaration":
		case "ExportDefaultDeclaration":
			return node.declaration
				? findDeclaredNameKind({ node: node.declaration, name })
				: undefined;
		case "ClassDeclaration":
		case "TSInterfaceDeclaration":
		case "TSTypeAliasDeclaration":
		case "TSEnumDeclaration":
			return node.id?.name === name ? "type" : undefined;
		case "FunctionDeclaration":
		case "TSDeclareFunction":
			return node.id?.name === name ? "value" : undefined;
		case "TSModuleDeclaration":
			return node.id.type === "Identifier" && node.id.name === name
				? "value"
				: undefined;
		case "VariableDeclaration":
			return node.declarations.some((declarator) => {
				return (
					declarator.id.type === "Identifier" &&
					declarator.id.name === name
				);
			})
				? "value"
				: undefined;
		case "ImportDeclaration": {
			const specifier = node.specifiers.find((candidate) => {
				return candidate.local.name === name;
			});
			if (specifier === undefined) {
				return undefined;
			}
			return node.importKind === "type" ||
				(specifier.type === "ImportSpecifier" &&
					specifier.importKind === "type")
				? "type"
				: "value";
		}
		default:
			return undefined;
	}
}

function findTopLevelNameKind(input: {
	readonly program: ESTree.Program;
	readonly name: string;
}): NameKind | undefined {
	for (const statement of input.program.body) {
		const kind = findDeclaredNameKind({
			node: statement,
			name: input.name,
		});
		if (kind !== undefined) {
			return kind;
		}
	}
	return undefined;
}

function getPropertyName(
	key: ESTree.PropertyKey,
	computed: boolean,
): ESTree.IdentifierName | ESTree.PrivateIdentifier | undefined {
	if (
		computed ||
		(key.type !== "Identifier" && key.type !== "PrivateIdentifier")
	) {
		return undefined;
	}
	return key;
}

export const rule = defineRule({
	meta: {
		type: "suggestion",
		docs: {
			description:
				"Require owned identifiers to use casing with unambiguous word boundaries.",
		},
		schema: [],
		messages: {
			[VALUE_MSG_ID]:
				"Owned name `{{name}}` must use unambiguous camel, Pascal, or snake casing.",
			[TYPE_MSG_ID]:
				"Type-like name `{{name}}` must use strict PascalCase without consecutive capitals.",
		},
	},
	create(ctx) {
		function checkValue(node: NamedIdentifier) {
			if (
				node.name !== "this" &&
				!testConvertibleValueName(node.name) &&
				!testInsideExternalDeclaration(node)
			) {
				ctx.report({
					node,
					messageId: VALUE_MSG_ID,
					data: { name: node.name },
				});
			}
		}

		function checkType(node: NamedIdentifier) {
			if (
				!testStrictPascalCase(node.name) &&
				!testInsideExternalDeclaration(node)
			) {
				ctx.report({
					node,
					messageId: TYPE_MSG_ID,
					data: { name: node.name },
				});
			}
		}

		function checkTypeParameter(node: ESTree.BindingIdentifier) {
			if (
				!testTypeParameterName(node.name) &&
				!testInsideExternalDeclaration(node)
			) {
				ctx.report({
					node,
					messageId: TYPE_MSG_ID,
					data: { name: node.name },
				});
			}
		}

		function checkBinding(node: ESTree.Node) {
			switch (node.type) {
				case "Identifier":
					checkValue(node);
					break;
				case "AssignmentPattern":
					checkBinding(node.left);
					break;
				case "ArrayPattern":
					for (const element of node.elements) {
						if (element) {
							checkBinding(element);
						}
					}
					break;
				case "ObjectPattern":
					for (const property of node.properties) {
						checkBinding(
							property.type === "RestElement"
								? property.argument
								: property.value,
						);
					}
					break;
				case "RestElement":
					checkBinding(node.argument);
					break;
				case "TSParameterProperty":
					checkBinding(node.parameter);
					break;
			}
		}

		function checkNamespaceName(
			name: ESTree.BindingIdentifier | ESTree.TSTypeName,
		) {
			switch (name.type) {
				case "Identifier":
					checkValue(name);
					return;
				case "TSQualifiedName":
					checkNamespaceName(name.left);
					checkValue(name.right);
					return;
				case "ThisExpression":
					return;
			}
		}

		function checkParameters(nodes: Array<ESTree.ParamPattern>) {
			for (const node of nodes) {
				checkBinding(node);
			}
		}

		function checkMember(
			node:
				| ESTree.MethodDefinition
				| ESTree.PropertyDefinition
				| ESTree.AccessorProperty,
		) {
			if (node.override) {
				return;
			}
			const name = getPropertyName(node.key, node.computed);
			if (name && name.name !== "constructor") {
				checkValue(name);
			}
		}

		return {
			VariableDeclarator(node) {
				checkBinding(node.id);
			},
			FunctionDeclaration(node) {
				if (node.id) {
					checkValue(node.id);
				}
				checkParameters(node.params);
			},
			FunctionExpression(node) {
				if (node.id) {
					checkValue(node.id);
				}
				checkParameters(node.params);
			},
			ArrowFunctionExpression(node) {
				checkParameters(node.params);
			},
			TSFunctionType(node) {
				checkParameters(node.params);
			},
			TSConstructorType(node) {
				checkParameters(node.params);
			},
			TSModuleDeclaration(node) {
				if (node.kind !== "global" && node.id.type !== "Literal") {
					checkNamespaceName(node.id);
				}
			},
			TSDeclareFunction(node) {
				if (node.id) {
					checkValue(node.id);
				}
				checkParameters(node.params);
			},
			TSEmptyBodyFunctionExpression(node) {
				if (node.id) {
					checkValue(node.id);
				}
				checkParameters(node.params);
			},
			CatchClause(node) {
				if (node.param) {
					checkBinding(node.param);
				}
			},
			ClassDeclaration(node) {
				if (node.id) {
					checkType(node.id);
				}
			},
			ClassExpression(node) {
				if (node.id) {
					checkType(node.id);
				}
			},
			MethodDefinition: checkMember,
			TSAbstractMethodDefinition: checkMember,
			PropertyDefinition: checkMember,
			TSAbstractPropertyDefinition: checkMember,
			AccessorProperty: checkMember,
			TSAbstractAccessorProperty: checkMember,
			TSTypeAliasDeclaration(node) {
				checkType(node.id);
			},
			TSInterfaceDeclaration(node) {
				checkType(node.id);
			},
			TSEnumDeclaration(node) {
				checkType(node.id);
			},
			TSEnumMember(node) {
				if (node.id.type === "Identifier") {
					checkValue(node.id);
				}
			},
			TSTypeParameter(node) {
				checkTypeParameter(node.name);
			},
			TSPropertySignature(node) {
				const name = getPropertyName(node.key, node.computed);
				if (name) {
					checkValue(name);
				}
			},
			TSMethodSignature(node) {
				const name = getPropertyName(node.key, node.computed);
				if (name) {
					checkValue(name);
				}
				checkParameters(node.params);
			},
			TSCallSignatureDeclaration(node) {
				checkParameters(node.params);
			},
			TSConstructSignatureDeclaration(node) {
				checkParameters(node.params);
			},
			TSIndexSignature(node) {
				checkParameters(node.parameters);
			},
			ImportDefaultSpecifier(node) {
				checkValue(node.local);
			},
			ImportNamespaceSpecifier(node) {
				checkValue(node.local);
			},
			ImportSpecifier(node) {
				if (
					node.imported.type !== "Identifier" ||
					node.local.name !== node.imported.name
				) {
					checkValue(node.local);
				}
			},
			ExportSpecifier(node) {
				const { local, exported } = node;
				// `default` names the module's default export, not an owned name.
				if (
					exported.type !== "Identifier" ||
					exported.name === "default" ||
					(local.type === "Identifier" &&
						local.name === exported.name)
				) {
					return;
				}
				const declaration = node.parent;
				const isTypeExport =
					node.exportKind === "type" ||
					(declaration.type === "ExportNamedDeclaration" &&
						(declaration.exportKind === "type" ||
							(declaration.source === null &&
								local.type === "Identifier" &&
								findTopLevelNameKind({
									program: ctx.sourceCode.ast,
									name: local.name,
								}) === "type")));
				if (isTypeExport) {
					checkType(exported);
				} else {
					checkValue(exported);
				}
			},
			ExportAllDeclaration(node) {
				if (node.exported?.type === "Identifier") {
					checkValue(node.exported);
				}
			},
		};
	},
});
