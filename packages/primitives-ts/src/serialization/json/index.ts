import type { Result } from "../../results/index.ts";
import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
} from "../../results/index.ts";

/** These options select formatting for JSON with recursively sorted object keys. */
export type JsonStringifyOptions = {
	readonly format:
		| { readonly kind: "compact" }
		| { readonly kind: "pretty"; readonly indent: string };
};

/** These errors describe JSON conversion failures and invalid formatting. */
export type JsonStringifyError =
	| { readonly kind: "invalid-indent" }
	| { readonly kind: "no-json-value" }
	| { readonly kind: "serialization-failed"; readonly cause: unknown };

/**
 * This function serializes with object keys sorted lexicographically by UTF-16
 * code units at every depth, including numeric-looking keys. Array order is preserved.
 * Pretty indentation must be a nonempty string of spaces and/or tabs.
 *
 * Native JSON.stringify converts the input once before ordering: toJSON and
 * getters retain their normal invocation behavior, undefined object properties
 * are omitted, array holes become null, and nonfinite numbers become null.
 * Cycles, unconverted bigint values, and throwing getters/toJSON return a typed
 * failure. A root value without a JSON representation returns no-json-value.
 *
 * Repeated references are valid. Native scalar encodings, including raw JSON
 * numeric literals on runtimes that support them, are preserved without parsing
 * numbers through a JavaScript Number. The operation retains the native JSON
 * text and an intermediate structure while ordering and formatting it.
 *
 * The function has no size or depth limit, so its memory and time grow with
 * the input. Use it only for trusted values.
 */
export function stringifyJsonStable(
	value: unknown,
	options: JsonStringifyOptions,
): Result<string, JsonStringifyError> {
	const indent =
		options.format.kind === "compact" ? "" : options.format.indent;
	if (options.format.kind === "pretty" && !/^[\t ]+$/.test(indent)) {
		return createErrorResult({ kind: "invalid-indent" });
	}
	const serialized = callWithResult(() => JSON.stringify(value));
	if (serialized.kind === "error") {
		return createErrorResult({
			kind: "serialization-failed",
			cause: serialized.error,
		});
	}
	if (serialized.value === undefined) {
		return createErrorResult({ kind: "no-json-value" });
	}
	return createSuccessResult(
		renderJson(parseSerializedJson(serialized.value), indent),
	);
}

type JsonNode =
	| { readonly kind: "scalar"; readonly text: string }
	| { readonly kind: "array"; readonly items: Array<JsonNode> }
	| { readonly kind: "object"; readonly entries: Map<string, JsonNode> };

type ParseFrame =
	| {
			readonly kind: "array";
			readonly node: Extract<JsonNode, { kind: "array" }>;
	  }
	| {
			readonly kind: "object-key";
			readonly node: Extract<JsonNode, { kind: "object" }>;
	  }
	| {
			readonly kind: "object-value";
			readonly node: Extract<JsonNode, { kind: "object" }>;
			readonly key: string;
	  };

function createJsonNode(token: string): JsonNode {
	return token === "["
		? { kind: "array", items: [] }
		: token === "{"
			? { kind: "object", entries: new Map() }
			: { kind: "scalar", text: token };
}

/** This parser reads only valid compact text produced by native JSON.stringify. */
function parseSerializedJson(text: string): JsonNode {
	const frames: Array<ParseFrame> = [];
	function pushFrame(node: JsonNode): void {
		switch (node.kind) {
			case "array":
				frames.push({ kind: "array", node });
				break;
			case "object":
				frames.push({ kind: "object-key", node });
				break;
			case "scalar":
				break;
		}
	}
	// Scalar tokens retain their exact spelling; only object keys need decoding for sorting.
	const tokens = text.matchAll(/"(?:[^"\\]|\\.)*"|[{}[\]:,]|[^{}[\]:,"]+/g);
	const first = tokens.next();
	if (first.done) {
		throw new Error("Native JSON output must contain a root value.");
	}
	const root = createJsonNode(first.value[0]);
	pushFrame(root);
	for (const [token] of tokens) {
		if (token === ":" || token === ",") {
			continue;
		}
		if (token === "}" || token === "]") {
			frames.pop();
			continue;
		}
		const parent = frames[frames.length - 1];
		if (parent === undefined) {
			throw new Error(
				"Native JSON output must contain exactly one root value.",
			);
		}
		if (parent.kind === "object-key") {
			frames[frames.length - 1] = {
				kind: "object-value",
				node: parent.node,
				key: JSON.parse(token) as string,
			};
			continue;
		}
		const node = createJsonNode(token);
		if (parent.kind === "array") {
			parent.node.items.push(node);
		} else {
			parent.node.entries.set(parent.key, node);
			frames[frames.length - 1] = {
				kind: "object-key",
				node: parent.node,
			};
		}
		pushFrame(node);
	}
	return root;
}

type RenderStep = string | { readonly node: JsonNode; readonly depth: number };

function renderJson(root: JsonNode, indent: string): string {
	const pending: Array<RenderStep> = [{ node: root, depth: 0 }];
	const output: Array<string> = [];
	while (pending.length !== 0) {
		const step = pending.pop()!;
		if (typeof step === "string") {
			output.push(step);
			continue;
		}
		const { node, depth } = step;
		if (node.kind === "scalar") {
			output.push(node.text);
			continue;
		}
		const keys =
			node.kind === "object"
				? [...node.entries.keys()].sort()
				: undefined;
		const count = node.kind === "array" ? node.items.length : keys!.length;
		const open = node.kind === "array" ? "[" : "{";
		const close = node.kind === "array" ? "]" : "}";
		output.push(open);
		if (count === 0) {
			output.push(close);
			continue;
		}
		const line = indent === "" ? "" : `\n${indent.repeat(depth + 1)}`;
		pending.push(
			indent === "" ? close : `\n${indent.repeat(depth)}${close}`,
		);
		for (let index = count - 1; index >= 0; index--) {
			const key = keys?.[index];
			pending.push({
				node:
					node.kind === "array"
						? node.items[index]!
						: node.entries.get(key!)!,
				depth: depth + 1,
			});
			if (key !== undefined) {
				pending.push(
					`${JSON.stringify(key)}:${indent === "" ? "" : " "}`,
				);
			}
			pending.push(`${index === 0 ? "" : ","}${line}`);
		}
	}
	return output.join("");
}
