import { defineRule, type ESTree, type SourceCode } from "@oxlint/plugins";

export const RULE_NAME = "no-complex-implicit-return";

const MSG_ID = "use_braces";

function findReturnedObjectLiteral(
	node: ESTree.Expression,
): ESTree.ObjectExpression | undefined {
	let current = node;
	while (
		current.type === "ParenthesizedExpression" ||
		current.type === "TSAsExpression" ||
		current.type === "TSSatisfiesExpression" ||
		current.type === "TSTypeAssertion" ||
		current.type === "TSNonNullExpression"
	) {
		current = current.expression;
	}
	return current.type === "ObjectExpression" ? current : undefined;
}

function getWrappingParenRange(
	source: SourceCode,
	node: ESTree.Node,
): [number, number] {
	let start = node.range[0];
	let end = node.range[1];
	let left = source.getTokenBefore(node);
	let right = source.getTokenAfter(node);
	while (left?.value === "(" && right?.value === ")") {
		start = left.range[0];
		end = right.range[1];
		left = source.getTokenBefore(left);
		right = source.getTokenAfter(right);
	}
	return [start, end];
}

// Parentheses around an arrow expression distinguish an object literal from
// a function body. An explicit return permits removing those parentheses,
// but the replacement must preserve comments inside them.
function testHidesComment(
	source: SourceCode,
	range: [number, number],
	node: ESTree.Node,
): boolean {
	return /[^\s()]/.test(
		source.text.slice(range[0], node.range[0]) +
			source.text.slice(node.range[1], range[1]),
	);
}

export const rule = defineRule({
	meta: {
		type: "suggestion",
		docs: {
			description:
				"Disallow implicit returns from arrow functions when the returned value is an object literal or an expression spanning multiple lines; single-line expressions may keep the implicit form.",
		},
		fixable: "code",
		schema: [],
		messages: {
			[MSG_ID]:
				"Arrow functions that return an object literal or an expression spanning multiple lines should use braces and an explicit return.",
		},
	},
	create(ctx) {
		const source = ctx.sourceCode;
		return {
			ArrowFunctionExpression(node) {
				const body = node.body;
				if (body.type === "BlockStatement") {
					return;
				}
				const isMultiLine = body.loc.start.line !== body.loc.end.line;
				const objectLiteral = findReturnedObjectLiteral(body);
				if (!isMultiLine && !objectLiteral) {
					return;
				}
				ctx.report({
					node: body,
					messageId: MSG_ID,
					fix(fixer) {
						const outer = getWrappingParenRange(source, body);
						const keep = testHidesComment(source, outer, body);
						const start = keep ? outer[0] : body.range[0];
						const end = keep ? outer[1] : body.range[1];
						// The object literal carries its own parens when a
						// TS operator wraps it, as in `({ a: 1 }) as const`.
						const inner = objectLiteral
							? getWrappingParenRange(source, objectLiteral)
							: undefined;
						const text =
							objectLiteral &&
							inner &&
							!testHidesComment(source, inner, objectLiteral)
								? source.text.slice(start, inner[0]) +
									source.getText(objectLiteral) +
									source.text.slice(inner[1], end)
								: source.text.slice(start, end);
						return fixer.replaceTextRange(
							outer,
							`{ return ${text}; }`,
						);
					},
				});
			},
		};
	},
});
