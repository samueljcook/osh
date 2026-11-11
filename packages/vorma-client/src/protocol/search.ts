import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "primitives-ts/results";
import { checkWellFormedUnicode } from "./path.ts";

/*
The search-parameter grammar over plain JSON values. Dots in a key separate
nesting. Reserved spellings start with a tilde: `~n` is null, `~a` an empty
array, `~o` an empty object, and `~s` prefixes a scalar that itself starts with
a tilde; `~s` alone is the empty string. Inside a key segment, `~0` is a tilde,
`~1` a dot, and `~2` the empty segment. A plain empty value means that nothing
was entered, as a form submits an empty input: an optional field is absent, a
nullable field is null, a required string is the empty string, and a required
field of another type is missing. Empty entries of an array or a record are
dropped.

The name `vorma-json` belongs to Vorma's wire protocol: the client sets and
strips it on page reads, and the server removes it from a page's query. The
name `dpl` belongs to Vercel's skew protection, and it is reserved only while
skew protection is on, which is when the server sends the client a deployment
ID: the client then sets and strips `dpl` as well, and the server removes it. A
top-level key with a reserved name cannot reach an application, so `buildHref`
(`href.ts`) reports a query that carries one as an error, and the server
rejects a search shape that declares one.

The Rust server (`crates/vorma/src/search.rs`) parses the same grammar. Each
shape that it derives is a shape here, and the serializer spells every JSON
value the way that the Rust parser reads it.
*/

const ESCAPE_PREFIX = "~";
const KEY_SEPARATOR = ".";
const ESCAPED_TILDE = "~0";
const ESCAPED_KEY_SEPARATOR = "~1";
const EMPTY_KEY_SEGMENT = "~2";
const NULL_MARKER = "~n";
const EMPTY_ARRAY_MARKER = "~a";
const EMPTY_OBJECT_MARKER = "~o";
const ESCAPED_SCALAR_PREFIX = "~s";

type Presence = { readonly optional?: true; readonly nullable?: true };

/** The URL description of a scalar: the `search_shape` form that the server sends. */
export type SearchScalarShape = Presence & {
	readonly kind: "scalar";
	readonly type: "string" | "number" | "boolean";
	readonly values?: ReadonlyArray<string>;
};

export type SearchShape =
	| SearchScalarShape
	| (Presence & {
			readonly kind: "object";
			readonly fields: Readonly<Record<string, SearchShape>>;
	  })
	| (Presence & {
			readonly kind: "array";
			readonly collection: "array";
			readonly item: SearchScalarShape;
	  })
	| (Presence & {
			readonly kind: "record";
			readonly collection: "record";
			readonly key: SearchScalarShape;
			readonly value: SearchShape;
	  });

export type SearchParseError =
	| { readonly kind: "missing-search-param"; readonly path: string }
	| {
			readonly kind: "invalid-search-param";
			readonly path: string;
			readonly expected: string;
	  };

export type SearchSerializeError = {
	readonly kind: "unsupported-search-value";
	readonly path: string;
	readonly msg: string;
};

function failMissingSearchParam(path: string): Result<never, SearchParseError> {
	return createErrorResult({ kind: "missing-search-param", path });
}

function failInvalidSearchParam(input: {
	readonly path: string;
	readonly expected: string;
}): Result<never, SearchParseError> {
	return createErrorResult({
		kind: "invalid-search-param",
		path: input.path,
		expected: input.expected,
	});
}

function checkRecord(
	value: unknown,
): value is Readonly<Record<string, unknown>> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Checks the structure of a `search_shape` that the server sent. */
export function readSearchShape(
	value: unknown,
	path = "$",
): Result<SearchShape, string> {
	if (!checkRecord(value)) {
		return createErrorResult(`${path} must be an object`);
	}
	const presence: { optional?: true; nullable?: true } = {};
	if (value["optional"] === true) {
		presence.optional = true;
	}
	if (value["nullable"] === true) {
		presence.nullable = true;
	}
	switch (value["kind"]) {
		case "scalar": {
			const scalar = readScalarShape(value, path);
			return scalar;
		}
		case "object": {
			const fields = value["fields"];
			if (!checkRecord(fields)) {
				return createErrorResult(`${path}.fields must be an object`);
			}
			const read: Record<string, SearchShape> = Object.create(null);
			for (const [name, field] of Object.entries(fields)) {
				const shape = readSearchShape(field, `${path}.fields.${name}`);
				if (shape.kind === "error") {
					return shape;
				}
				read[name] = shape.value;
			}
			return createSuccessResult({
				kind: "object",
				fields: read,
				...presence,
			});
		}
		case "array": {
			const item = readScalarShape(value["item"], `${path}.item`);
			if (item.kind === "error") {
				return item;
			}
			return createSuccessResult({
				kind: "array",
				collection: "array",
				item: item.value,
				...presence,
			});
		}
		case "record": {
			const key = readScalarShape(value["key"], `${path}.key`);
			if (key.kind === "error") {
				return key;
			}
			const entry = readSearchShape(value["value"], `${path}.value`);
			if (entry.kind === "error") {
				return entry;
			}
			return createSuccessResult({
				kind: "record",
				collection: "record",
				key: key.value,
				value: entry.value,
				...presence,
			});
		}
		default:
			return createErrorResult(
				`${path}.kind must be a search shape kind`,
			);
	}
}

function readScalarShape(
	value: unknown,
	path: string,
): Result<SearchScalarShape, string> {
	if (!checkRecord(value) || value["kind"] !== "scalar") {
		return createErrorResult(`${path} must be a scalar shape`);
	}
	const type = value["type"];
	if (type !== "string" && type !== "number" && type !== "boolean") {
		return createErrorResult(
			`${path}.type must be string, number, or boolean`,
		);
	}
	const values = value["values"];
	if (
		values !== undefined &&
		(!Array.isArray(values) ||
			!values.every((entry) => typeof entry === "string"))
	) {
		return createErrorResult(`${path}.values must be an array of strings`);
	}
	return createSuccessResult({
		kind: "scalar",
		type,
		...(values === undefined
			? {}
			: { values: values as ReadonlyArray<string> }),
		...(value["nullable"] === true ? { nullable: true as const } : {}),
		...(value["optional"] === true ? { optional: true as const } : {}),
	});
}

/**
 * Sets an own property of a parsed object. A name such as `__proto__` becomes
 * an own data property and leaves the object's prototype unchanged, so a field
 * or map key named like an `Object.prototype` member is only ever the caller's
 * data.
 */
function setOwn(target: object, name: string, value: unknown): void {
	Object.defineProperty(target, name, {
		value,
		enumerable: true,
		writable: true,
		configurable: true,
	});
}

/** Spells one key segment: a tilde as `~0`, a dot as `~1`, and the empty segment as `~2`. */
export function encodeSearchKeySegment(segment: string): string {
	return segment === ""
		? EMPTY_KEY_SEGMENT
		: segment
				.replaceAll(ESCAPE_PREFIX, ESCAPED_TILDE)
				.replaceAll(KEY_SEPARATOR, ESCAPED_KEY_SEPARATOR);
}

/**
 * Spells the text of a string scalar so that the grammar reads the same text
 * back: text that starts with a tilde gets the `~s` prefix. Empty text stays
 * empty, which the grammar reads as nothing entered.
 */
export function escapeSearchScalarText(text: string): string {
	return text.startsWith(ESCAPE_PREFIX)
		? `${ESCAPED_SCALAR_PREFIX}${text}`
		: text;
}

function appendKey(input: {
	readonly parent: string;
	readonly segment: string;
}): string {
	const { parent, segment } = input;
	return parent === "" ? segment : `${parent}${KEY_SEPARATOR}${segment}`;
}

/**
 * Parses a query into the value that its search shape describes. Parsed
 * objects are ordinary objects. For a given shape, the work and the memory are
 * linear in the length of the query.
 */
export function parseSearch(input: {
	readonly shape: SearchShape;
	readonly params: URLSearchParams;
}): Result<unknown, SearchParseError> {
	return createQueryIndex([...input.params]).parse({
		shape: input.shape,
		path: "",
		node: ROOT_NODE,
	});
}

/**
 * A node of the query index: a key prefix that the query contains, by its
 * index in the index's arrays.
 */
type QueryNode = number;

/** The empty prefix, which is the node of the empty key. */
const ROOT_NODE: QueryNode = 0;

/**
 * Where a parse reads: the shape, the key path that errors name, and the node
 * of that path, which is undefined when the query contains no key with that
 * prefix.
 */
type ParsePosition<Shape extends SearchShape = SearchShape> = {
	readonly shape: Shape;
	readonly path: string;
	readonly node: QueryNode | undefined;
};

type QueryIndex = {
	readonly parse: (
		position: ParsePosition,
	) => Result<unknown, SearchParseError>;
};

/**
 * Indexes a query's keys as a tree of key segments. Each distinct prefix is a
 * node with an integer identifier, and a node's children are keyed by their
 * segment, so the index is linear in the length of the query. The empty key
 * belongs to the root. A key that starts with an empty segment has that
 * segment as a child of the root, and the rest of the key below it, so the
 * key's later segments are not the root's children.
 */
function createQueryIndex(
	entries: ReadonlyArray<readonly [string, string]>,
): QueryIndex {
	const childrenByNode: Array<Map<string, QueryNode> | undefined> = [
		undefined,
	];
	const valuesByNode: Array<Array<string>> = [[]];
	const nodeByKey = new Map<string, QueryNode>();
	function insertKey(key: string): QueryNode {
		let parent = ROOT_NODE;
		let start = 0;
		for (;;) {
			const separator = key.indexOf(KEY_SEPARATOR, start);
			const segment = key.slice(
				start,
				separator === -1 ? key.length : separator,
			);
			let children = childrenByNode[parent];
			if (children === undefined) {
				children = new Map();
				childrenByNode[parent] = children;
			}
			let child = children.get(segment);
			if (child === undefined) {
				child = valuesByNode.length;
				valuesByNode.push([]);
				childrenByNode.push(undefined);
				children.set(segment, child);
			}
			if (separator === -1) {
				return child;
			}
			parent = child;
			start = separator + KEY_SEPARATOR.length;
		}
	}
	for (const [key, value] of entries) {
		let node = nodeByKey.get(key);
		if (node === undefined) {
			node = key === "" ? ROOT_NODE : insertKey(key);
			nodeByKey.set(key, node);
		}
		valuesByNode[node]?.push(value);
	}
	function getValuesAt(node: QueryNode | undefined): ReadonlyArray<string> {
		return node === undefined ? [] : (valuesByNode[node] ?? []);
	}
	function getEnteredValues(
		node: QueryNode | undefined,
	): ReadonlyArray<string> {
		return getValuesAt(node).filter((value) => value !== "");
	}
	function getChildren(
		node: QueryNode | undefined,
	): ReadonlyMap<string, QueryNode> | undefined {
		return node === undefined ? undefined : childrenByNode[node];
	}
	function checkChildren(node: QueryNode | undefined): boolean {
		return getChildren(node) !== undefined;
	}
	function findChild(input: {
		readonly node: QueryNode | undefined;
		readonly segment: string;
	}): QueryNode | undefined {
		return getChildren(input.node)?.get(input.segment);
	}
	// A scalar has no child keys, so a key below a scalar's key is not the
	// scalar's.
	function checkChildKeys(position: ParsePosition): boolean {
		return position.shape.kind !== "scalar" && checkChildren(position.node);
	}
	function checkEntersNothing(position: ParsePosition): boolean {
		if (checkChildKeys(position)) {
			return false;
		}
		const at = getValuesAt(position.node);
		return position.shape.kind === "scalar"
			? at[0] === ""
			: at.length > 0 && at.every((value) => value === "");
	}
	function checkPresent(position: ParsePosition): boolean {
		return (
			(getValuesAt(position.node).length > 0 ||
				checkChildKeys(position)) &&
			!checkEntersNothing(position)
		);
	}
	function parseObject(
		position: ParsePosition<Extract<SearchShape, { kind: "object" }>>,
	): Result<unknown, SearchParseError> {
		const { path, node } = position;
		const entered = getEnteredValues(node);
		if (
			entered.length > 0 &&
			(!(entered.length === 1 && entered[0] === EMPTY_OBJECT_MARKER) ||
				checkChildren(node))
		) {
			return failInvalidSearchParam({
				path,
				expected: "an unambiguous empty-object marker or child fields",
			});
		}
		const object: Record<string, unknown> = {};
		for (const [name, shape] of Object.entries(position.shape.fields)) {
			const segment = encodeSearchKeySegment(name);
			const field: ParsePosition = {
				shape,
				path: appendKey({ parent: path, segment }),
				node: findChild({ node, segment }),
			};
			if (shape.optional === true && !checkPresent(field)) {
				continue;
			}
			const parsed = parse(field);
			if (parsed.kind === "error") {
				return parsed;
			}
			setOwn(object, name, parsed.value);
		}
		return createSuccessResult(object);
	}
	function parseArray(
		position: ParsePosition<Extract<SearchShape, { kind: "array" }>>,
	): Result<unknown, SearchParseError> {
		const { path, node } = position;
		const item = position.shape.item;
		const entered = getEnteredValues(node);
		if (
			entered.length === 0 ||
			(entered.length === 1 && entered[0] === EMPTY_ARRAY_MARKER)
		) {
			return createSuccessResult([]);
		}
		if (entered.includes(EMPTY_ARRAY_MARKER)) {
			return failInvalidSearchParam({
				path,
				expected: "an array without a mixed empty-array marker",
			});
		}
		const items: Array<unknown> = [];
		for (const raw of entered) {
			const parsed = parseScalar({ scalar: item, raw, path });
			if (parsed.kind === "error") {
				return parsed;
			}
			items.push(parsed.value);
		}
		return createSuccessResult(items);
	}
	function parseRecord(
		position: ParsePosition<Extract<SearchShape, { kind: "record" }>>,
	): Result<unknown, SearchParseError> {
		const { path, node } = position;
		const keyValues = position.shape.key.values;
		const entered = getEnteredValues(node);
		if (
			entered.length === 1 &&
			entered[0] === EMPTY_OBJECT_MARKER &&
			!checkChildren(node)
		) {
			return createSuccessResult({});
		}
		if (entered.length > 0) {
			return failInvalidSearchParam({
				path,
				expected: "an empty-object marker or dotted map entries",
			});
		}
		const record: Record<string, unknown> = {};
		for (const [segment, child] of getChildren(node) ?? []) {
			if (segment === "") {
				continue;
			}
			const key = decodeKeySegment({ encoded: segment, path });
			if (key.kind === "error") {
				return key;
			}
			if (keyValues !== undefined && !keyValues.includes(key.value)) {
				return failInvalidSearchParam({
					path,
					expected: "a declared enum variant",
				});
			}
			const entry: ParsePosition = {
				shape: position.shape.value,
				path: appendKey({ parent: path, segment }),
				node: child,
			};
			if (checkEntersNothing(entry)) {
				continue;
			}
			const parsed = parse(entry);
			if (parsed.kind === "error") {
				return parsed;
			}
			setOwn(record, key.value, parsed.value);
		}
		return createSuccessResult(record);
	}
	function parse(position: ParsePosition): Result<unknown, SearchParseError> {
		const { shape, path, node } = position;
		if (shape.nullable === true) {
			if (!checkPresent(position)) {
				return createSuccessResult(null);
			}
			const exact = getValuesAt(node);
			if (shape.kind === "scalar") {
				if (exact[0] === NULL_MARKER) {
					return createSuccessResult(null);
				}
			} else if (exact.includes(NULL_MARKER)) {
				return exact.length === 1 && !checkChildren(node)
					? createSuccessResult(null)
					: failInvalidSearchParam({
							path,
							expected: "an unambiguous null marker",
						});
			}
		}
		switch (shape.kind) {
			case "scalar": {
				const raw = getValuesAt(node)[0];
				if (raw === undefined) {
					return failMissingSearchParam(path);
				}
				// Nothing entered is the empty string only for a string.
				if (raw === "" && shape.type !== "string") {
					return failMissingSearchParam(path);
				}
				return parseScalar({ scalar: shape, raw, path });
			}
			case "object":
				return parseObject({ shape, path, node });
			case "array":
				return parseArray({ shape, path, node });
			case "record":
				return parseRecord({ shape, path, node });
		}
	}
	return { parse };
}

function parseScalar(input: {
	readonly scalar: SearchScalarShape;
	readonly raw: string;
	readonly path: string;
}): Result<unknown, SearchParseError> {
	const { scalar, raw, path } = input;
	if (raw === NULL_MARKER && scalar.nullable === true) {
		return createSuccessResult(null);
	}
	const text = decodeScalar({ raw, path });
	if (text.kind === "error") {
		return text;
	}
	switch (scalar.type) {
		case "string":
			return scalar.values !== undefined &&
				!scalar.values.includes(text.value)
				? failInvalidSearchParam({
						path,
						expected: "a declared enum variant",
					})
				: createSuccessResult(text.value);
		case "boolean": {
			// The serializer writes true and false. Forms and people also write 1
			// and 0, and a checked checkbox without a value attribute sends on.
			const lower = text.value.toLowerCase();
			if (lower === "true" || lower === "1" || lower === "on") {
				return createSuccessResult(true);
			}
			if (lower === "false" || lower === "0") {
				return createSuccessResult(false);
			}
			return failInvalidSearchParam({
				path,
				expected: "true, false, 1, 0, or on",
			});
		}
		case "number":
			return parseNumber({ text: text.value, path });
	}
}

const DECIMAL_NUMBER = /^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?$/;

function parseNumber(input: {
	readonly text: string;
	readonly path: string;
}): Result<number, SearchParseError> {
	const { text, path } = input;
	const number = DECIMAL_NUMBER.test(text) ? Number(text) : Number.NaN;
	// Adding zero turns negative zero into zero, as the shared vectors require.
	return Number.isFinite(number)
		? createSuccessResult(number + 0)
		: failInvalidSearchParam({ path, expected: "a finite decimal number" });
}

function decodeKeySegment(input: {
	readonly encoded: string;
	readonly path: string;
}): Result<string, SearchParseError> {
	const { encoded, path } = input;
	if (encoded === EMPTY_KEY_SEGMENT) {
		return createSuccessResult("");
	}
	let decoded = "";
	for (let index = 0; index < encoded.length; index++) {
		const character = encoded.charAt(index);
		if (character !== ESCAPE_PREFIX) {
			decoded += character;
			continue;
		}
		index += 1;
		const escape = encoded.charAt(index);
		if (escape === "0") {
			decoded += ESCAPE_PREFIX;
		} else if (escape === "1") {
			decoded += KEY_SEPARATOR;
		} else {
			return failInvalidSearchParam({
				path,
				expected: "a well-formed key escape",
			});
		}
	}
	return createSuccessResult(decoded);
}

function decodeScalar(input: {
	readonly raw: string;
	readonly path: string;
}): Result<string, SearchParseError> {
	const { raw, path } = input;
	if (raw.startsWith(ESCAPED_SCALAR_PREFIX)) {
		const escaped = raw.slice(ESCAPED_SCALAR_PREFIX.length);
		// The escape alone is the empty string.
		return escaped === "" || escaped.startsWith(ESCAPE_PREFIX)
			? createSuccessResult(escaped)
			: failInvalidSearchParam({
					path,
					expected: "a well-formed escaped scalar",
				});
	}
	return raw.startsWith(ESCAPE_PREFIX)
		? failInvalidSearchParam({
				path,
				expected: "a scalar, not a reserved marker",
			})
		: createSuccessResult(raw);
}

/**
 * Spells a JSON-like value in the grammar and returns the query text without
 * its leading question mark. An `undefined` field is omitted. A value without a
 * JSON spelling (a function, a symbol, a bigint, a class instance, a number
 * that is not finite, or `undefined` in an array) is an error. A value that
 * the grammar cannot spell is an error too: an array or an object inside an
 * array, and a string or a field name that is not well-formed Unicode, which
 * URL encoding would change. Cycles and throwing property access return a
 * serialization error without exposing the thrown value.
 */
export function serializeSearch(
	value: unknown,
): Result<string, SearchSerializeError> {
	const entries: Array<readonly [string, string]> = [];
	const written = callWithResult(() => {
		return appendValue({
			entries,
			key: "",
			path: [],
			value,
			ancestors: new Set(),
		});
	});
	if (written.kind === "error") {
		return failUnsupportedSearchValue(
			[],
			"Search values could not be read or serialized.",
		);
	}
	if (written.value.kind === "error") {
		return written.value;
	}
	const params = new URLSearchParams();
	for (const [key, item] of entries) {
		params.append(key, item);
	}
	return createSuccessResult(params.toString());
}

function failUnsupportedSearchValue(
	path: ReadonlyArray<string | number>,
	msg: string,
): Result<never, SearchSerializeError> {
	return createErrorResult({
		kind: "unsupported-search-value",
		path: path.join("."),
		msg,
	});
}

function appendValue(input: {
	readonly entries: Array<readonly [string, string]>;
	readonly key: string;
	readonly path: ReadonlyArray<string | number>;
	readonly value: unknown;
	readonly ancestors: Set<object>;
}): Result<void, SearchSerializeError> {
	const { entries, key, path, value, ancestors } = input;
	if (value === null) {
		entries.push([key, NULL_MARKER]);
		return createSuccessResult(undefined);
	}
	switch (typeof value) {
		case "boolean":
			entries.push([key, String(value)]);
			return createSuccessResult(undefined);
		case "number":
			if (!Number.isFinite(value)) {
				return failUnsupportedSearchValue(
					path,
					"numbers must be finite",
				);
			}
			entries.push([key, String(value)]);
			return createSuccessResult(undefined);
		case "string": {
			if (checkWellFormedUnicode(value).kind === "error") {
				return failUnsupportedSearchValue(
					path,
					"strings must be well-formed Unicode",
				);
			}
			// A plain empty value means that nothing was entered; the escape alone is the empty string.
			entries.push([
				key,
				value === ""
					? ESCAPED_SCALAR_PREFIX
					: escapeSearchScalarText(value),
			]);
			return createSuccessResult(undefined);
		}
		case "object":
			break;
		default:
			return failUnsupportedSearchValue(
				path,
				`a ${typeof value} has no URL spelling`,
			);
	}
	if (ancestors.has(value)) {
		return failUnsupportedSearchValue(path, "cycles have no URL spelling");
	}
	ancestors.add(value);
	if (Array.isArray(value)) {
		if (value.length === 0) {
			entries.push([key, EMPTY_ARRAY_MARKER]);
			ancestors.delete(value);
			return createSuccessResult(undefined);
		}
		for (const [index, item] of value.entries()) {
			if (item === undefined) {
				return failUnsupportedSearchValue(
					[...path, index],
					"arrays cannot contain undefined",
				);
			}
			// An array repeats its key once for each item, so an item has no
			// key of its own for nested values.
			if (typeof item === "object" && item !== null) {
				return failUnsupportedSearchValue(
					[...path, index],
					"arrays can contain only scalars and null",
				);
			}
			const written = appendValue({
				entries,
				key,
				path: [...path, index],
				value: item,
				ancestors,
			});
			if (written.kind === "error") {
				return written;
			}
		}
		ancestors.delete(value);
		return createSuccessResult(undefined);
	}
	const prototype = Object.getPrototypeOf(value);
	if (prototype !== Object.prototype && prototype !== null) {
		return failUnsupportedSearchValue(
			path,
			"only plain objects have a URL spelling",
		);
	}
	const fields = Object.entries(value).filter(
		([, child]) => child !== undefined,
	);
	if (fields.length === 0) {
		entries.push([key, EMPTY_OBJECT_MARKER]);
		ancestors.delete(value);
		return createSuccessResult(undefined);
	}
	fields.sort(([left], [right]) =>
		left < right ? -1 : left > right ? 1 : 0,
	);
	for (const [name, child] of fields) {
		if (checkWellFormedUnicode(name).kind === "error") {
			return failUnsupportedSearchValue(
				[...path, name],
				"field names must be well-formed Unicode",
			);
		}
		const written = appendValue({
			entries,
			key: appendKey({
				parent: key,
				segment: encodeSearchKeySegment(name),
			}),
			path: [...path, name],
			value: child,
			ancestors,
		});
		if (written.kind === "error") {
			return written;
		}
	}
	ancestors.delete(value);
	return createSuccessResult(undefined);
}
