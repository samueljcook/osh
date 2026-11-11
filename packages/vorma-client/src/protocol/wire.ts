import type {
	CompiledNestedPattern,
	PatternSegment,
} from "primitives-ts/path-matcher";
import {
	createErrorResult,
	createSuccessResult,
	type Result,
} from "primitives-ts/results";
import type { RedirectStatus } from "./constants.ts";
import { readSearchShape, type SearchShape } from "./search.ts";

/*
The payloads that the server writes, read from `JSON.parse` output. Every
reader checks the structure that the client relies on and returns an error for
a payload that is not the server's shape; a reader asserts a decoded type for
a value only after it has checked the value's structure. A field that the server omits when
it is empty is read as empty, as the server's omission rules define. Property
names on the wire are snake case, and the decoded types use the API's casing.
*/

export type PayloadReadError = {
	readonly kind: "payload-read-error";
	readonly path: string;
	readonly expected: string;
};

export type HeadEl = {
	readonly tag: string;
	readonly attrs?: Readonly<Record<string, string>>;
	readonly html?: string;
};

/**
 * A raw HTML head item. Its key is 32 lowercase hexadecimal characters, which
 * the server computes; equal items have equal keys.
 */
export type HeadRawHtml = { readonly rawHtml: string; readonly key: string };

/** Whether the text has the form of a raw HTML item's key: 32 lowercase hexadecimal characters. */
export function checkRawHtmlKey(text: string): boolean {
	return /^[0-9a-f]{32}$/.test(text);
}

export type HeadSections = {
	readonly title: HeadEl | undefined;
	readonly meta: ReadonlyArray<HeadEl>;
	readonly rest: ReadonlyArray<HeadEl | HeadRawHtml>;
};

export type DocumentAttrs = {
	readonly html: Readonly<Record<string, string>>;
	readonly head: Readonly<Record<string, string>>;
	readonly body: Readonly<Record<string, string>>;
};

/** A failure that the server answered: its status and, when public, its declared error or rejection. */
export type ServerFailure =
	| {
			readonly kind: "declared-error";
			readonly status: number;
			readonly declaredError: unknown;
	  }
	| {
			readonly kind: "rejection";
			readonly status: number;
			readonly clientMsg: string;
	  }
	| { readonly kind: "undeclared-failure"; readonly status: number };

/** A page's server failure with the position of the first position that did not render. */
export type PageServerFailure = ServerFailure & {
	readonly positionIndex: number;
};

/** A field of a route record that the server writes or omits, so that a present `null` differs from an absent field. */
export type RouteRecordField =
	| { readonly kind: "absent" }
	| { readonly kind: "present"; readonly value: unknown };

export type RouteRecord = {
	readonly pattern: string;
	readonly params: Readonly<Record<string, string>>;
	readonly splatValues: ReadonlyArray<string>;
	readonly compiledPattern: CompiledNestedPattern;
	readonly searchShape: SearchShape | undefined;
	/** The search that the server parsed for the position's loader; the server writes it for a position with a search shape. */
	readonly searchInput: RouteRecordField;
	/** The view module; null for a view without a client definition. */
	readonly moduleUrl: string | null;
	/** The loader output; absent for a position without one, and present as `null` for a JSON null output. */
	readonly loaderOutput: RouteRecordField;
};

export type PagePayload = {
	readonly routes: ReadonlyArray<RouteRecord>;
	readonly failure: PageServerFailure | undefined;
};

/** The request destination of a static public asset that a page preloads, as a preload link's `as` attribute spells it. */
export type StaticPublicAssetDestination = "image" | "font";

/** A static public asset that a page preloads: its URL and its request destination. */
export type StaticPublicAssetPreload = {
	readonly url: string;
	readonly destination: StaticPublicAssetDestination;
};

export type NavPayload = PagePayload & {
	readonly head: HeadSections;
	readonly documentAttrs: DocumentAttrs;
	readonly modulePreloadUrls: ReadonlyArray<string>;
	readonly cssBundles: ReadonlyArray<string>;
	/** The static public assets that the page preloads, in the page's order, each URL once. */
	readonly staticPublicAssetPreloads: ReadonlyArray<StaticPublicAssetPreload>;
};

export type BootstrapPayload = PagePayload & {
	readonly clientBuildId: string;
	readonly documentAttrs: DocumentAttrs;
	readonly devMode: boolean;
	readonly deploymentId: string | undefined;
};

function failRead(input: {
	readonly path: string;
	readonly expected: string;
}): Result<never, PayloadReadError> {
	return createErrorResult({
		kind: "payload-read-error",
		path: input.path,
		expected: input.expected,
	});
}

type JsonRecord = Readonly<Record<string, unknown>>;

function readRecord(
	value: unknown,
	path: string,
): Result<JsonRecord, PayloadReadError> {
	return typeof value === "object" && value !== null && !Array.isArray(value)
		? createSuccessResult(value as JsonRecord)
		: failRead({ path, expected: "an object" });
}

function readStringRecord(
	value: unknown,
	path: string,
): Result<Readonly<Record<string, string>>, PayloadReadError> {
	const record = readRecord(value, path);
	if (record.kind === "error") {
		return record;
	}
	for (const [name, entry] of Object.entries(record.value)) {
		if (typeof entry !== "string") {
			return failRead({ path: `${path}.${name}`, expected: "a string" });
		}
	}
	return createSuccessResult(
		record.value as Readonly<Record<string, string>>,
	);
}

function readStringArray(
	value: unknown,
	path: string,
): Result<ReadonlyArray<string>, PayloadReadError> {
	if (!Array.isArray(value)) {
		return failRead({ path, expected: "an array" });
	}
	for (const [index, entry] of value.entries()) {
		if (typeof entry !== "string") {
			return failRead({
				path: `${path}[${index}]`,
				expected: "a string",
			});
		}
	}
	return createSuccessResult(value as ReadonlyArray<string>);
}

function readOptionalStringArray(
	record: JsonRecord,
	name: string,
): Result<ReadonlyArray<string>, PayloadReadError> {
	return record[name] === undefined
		? createSuccessResult([])
		: readStringArray(record[name], `$.${name}`);
}

const REDIRECT_STATUSES: ReadonlyMap<string, RedirectStatus> = new Map([
	["302", 302],
	["303", 303],
	["307", 307],
	["308", 308],
]);

/** Reads the redirect status header, which the server writes as the status's decimal digits. */
export function readRedirectStatus(
	value: string | null,
): Result<RedirectStatus, PayloadReadError> {
	const status = value === null ? undefined : REDIRECT_STATUSES.get(value);
	return status === undefined
		? failRead({ path: "$", expected: "one of 302, 303, 307, or 308" })
		: createSuccessResult(status);
}

/**
 * Reads the segments of a compiled pattern, in the order that the path matcher
 * requires: a splat or index segment only at the end, a static segment with a
 * value, and dynamic segments with distinct, nonempty param names.
 */
function readSegments(
	value: unknown,
	path: string,
): Result<ReadonlyArray<PatternSegment>, PayloadReadError> {
	if (!Array.isArray(value)) {
		return failRead({ path, expected: "an array" });
	}
	const segments: Array<PatternSegment> = [];
	const paramNames = new Set<string>();
	for (const [index, entry] of value.entries()) {
		const segmentPath = `${path}[${index}]`;
		const previous = segments.at(-1);
		if (previous?.kind === "splat" || previous?.kind === "index") {
			return failRead({
				path: segmentPath,
				expected: "no segment after a splat or index segment",
			});
		}
		const record = readRecord(entry, segmentPath);
		if (record.kind === "error") {
			return record;
		}
		const kind = record.value["kind"];
		if (kind === "static") {
			const staticValue = record.value["value"];
			if (typeof staticValue !== "string" || staticValue === "") {
				return failRead({
					path: `${segmentPath}.value`,
					expected: "a nonempty string",
				});
			}
			segments.push({ kind, value: staticValue });
		} else if (kind === "dynamic") {
			const paramName = record.value["paramName"];
			if (
				typeof paramName !== "string" ||
				paramName === "" ||
				paramNames.has(paramName)
			) {
				return failRead({
					path: `${segmentPath}.paramName`,
					expected:
						"a nonempty string that no other segment of the pattern names",
				});
			}
			paramNames.add(paramName);
			segments.push({ kind, paramName });
		} else if (kind === "splat" || kind === "index") {
			segments.push({ kind });
		} else {
			return failRead({
				path: `${segmentPath}.kind`,
				expected: "a pattern segment kind",
			});
		}
	}
	return createSuccessResult(segments);
}

function readRouteRecord(
	value: unknown,
	path: string,
): Result<RouteRecord, PayloadReadError> {
	const record = readRecord(value, path);
	if (record.kind === "error") {
		return record;
	}
	const fields = record.value;
	const pattern = fields["pattern"];
	if (typeof pattern !== "string") {
		return failRead({ path: `${path}.pattern`, expected: "a string" });
	}
	const params =
		fields["params"] === undefined
			? createSuccessResult({} as Readonly<Record<string, string>>)
			: readStringRecord(fields["params"], `${path}.params`);
	if (params.kind === "error") {
		return params;
	}
	const splatValues =
		fields["splat_values"] === undefined
			? createSuccessResult([] as ReadonlyArray<string>)
			: readStringArray(fields["splat_values"], `${path}.splat_values`);
	if (splatValues.kind === "error") {
		return splatValues;
	}
	const compiled = readRecord(
		fields["compiled_pattern"],
		`${path}.compiled_pattern`,
	);
	if (compiled.kind === "error") {
		return compiled;
	}
	const compiledPatternText = compiled.value["pattern"];
	if (
		compiledPatternText !== undefined &&
		typeof compiledPatternText !== "string"
	) {
		return failRead({
			path: `${path}.compiled_pattern.pattern`,
			expected: "a string",
		});
	}
	const segments = readSegments(
		compiled.value["segments"],
		`${path}.compiled_pattern.segments`,
	);
	if (segments.kind === "error") {
		return segments;
	}
	let searchShape: SearchShape | undefined;
	if (fields["search_shape"] !== undefined) {
		const shape = readSearchShape(
			fields["search_shape"],
			`${path}.search_shape`,
		);
		if (shape.kind === "error") {
			return failRead({
				path: `${path}.search_shape`,
				expected: shape.error,
			});
		}
		searchShape = shape.value;
	}
	const moduleUrl = fields["import_url"];
	if (moduleUrl !== null && typeof moduleUrl !== "string") {
		return failRead({
			path: `${path}.import_url`,
			expected: "a string or null",
		});
	}
	return createSuccessResult({
		pattern,
		params: params.value,
		splatValues: splatValues.value,
		compiledPattern: {
			kind: "nested",
			pattern: compiledPatternText ?? pattern,
			segments: segments.value,
		},
		searchShape,
		searchInput: readRouteRecordField(fields, "search_input"),
		moduleUrl,
		loaderOutput: readRouteRecordField(fields, "loader_output"),
	});
}

function readRouteRecordField(
	fields: JsonRecord,
	name: string,
): RouteRecordField {
	return Object.hasOwn(fields, name)
		? { kind: "present", value: fields[name] }
		: { kind: "absent" };
}

function readFailureFields(
	fields: JsonRecord,
	path: string,
): Result<ServerFailure, PayloadReadError> {
	const status = fields["status"];
	if (
		typeof status !== "number" ||
		!Number.isInteger(status) ||
		status < 100 ||
		status > 599
	) {
		return failRead({
			path: `${path}.status`,
			expected: "an HTTP status from 100 to 599",
		});
	}
	if (Object.hasOwn(fields, "declared_error")) {
		return createSuccessResult({
			kind: "declared-error",
			status,
			declaredError: fields["declared_error"],
		});
	}
	if (Object.hasOwn(fields, "rejection")) {
		const rejection = readRecord(fields["rejection"], `${path}.rejection`);
		if (rejection.kind === "error") {
			return rejection;
		}
		const clientMsg = rejection.value["client_msg"];
		if (typeof clientMsg !== "string") {
			return failRead({
				path: `${path}.rejection.client_msg`,
				expected: "a string",
			});
		}
		return createSuccessResult({ kind: "rejection", status, clientMsg });
	}
	return createSuccessResult({ kind: "undeclared-failure", status });
}

function readPagePayload(
	fields: JsonRecord,
): Result<PagePayload, PayloadReadError> {
	const rawRoutes = fields["routes"];
	if (!Array.isArray(rawRoutes)) {
		return failRead({ path: "$.routes", expected: "an array" });
	}
	const routes: Array<RouteRecord> = [];
	for (const [index, rawRoute] of rawRoutes.entries()) {
		const route = readRouteRecord(rawRoute, `$.routes[${index}]`);
		if (route.kind === "error") {
			return route;
		}
		routes.push(route.value);
	}
	let failure: PageServerFailure | undefined;
	if (fields["failure"] !== undefined) {
		const record = readRecord(fields["failure"], "$.failure");
		if (record.kind === "error") {
			return record;
		}
		// The failing position follows the positions that rendered, so its
		// index is at most the number of route records.
		const index = record.value["index"];
		if (
			typeof index !== "number" ||
			!Number.isInteger(index) ||
			index < 0 ||
			index > routes.length
		) {
			return failRead({
				path: "$.failure.index",
				expected: `an integer from 0 to ${routes.length}`,
			});
		}
		const read = readFailureFields(record.value, "$.failure");
		if (read.kind === "error") {
			return read;
		}
		failure = { ...read.value, positionIndex: index };
	}
	return createSuccessResult({ routes, failure });
}

function readElement(
	value: unknown,
	path: string,
): Result<HeadEl, PayloadReadError> {
	const record = readRecord(value, path);
	if (record.kind === "error") {
		return record;
	}
	const tag = record.value["tag"];
	if (typeof tag !== "string") {
		return failRead({ path: `${path}.tag`, expected: "a string" });
	}
	const html = record.value["html"];
	if (html !== undefined && typeof html !== "string") {
		return failRead({ path: `${path}.html`, expected: "a string" });
	}
	const attrs =
		record.value["attrs"] === undefined
			? undefined
			: readStringRecord(record.value["attrs"], `${path}.attrs`);
	if (attrs?.kind === "error") {
		return attrs;
	}
	return createSuccessResult({
		tag,
		...(attrs === undefined ? {} : { attrs: attrs.value }),
		...(html === undefined ? {} : { html }),
	});
}

function readHead(value: unknown): Result<HeadSections, PayloadReadError> {
	const record = readRecord(value, "$.head");
	if (record.kind === "error") {
		return record;
	}
	let title: HeadEl | undefined;
	if (record.value["title"] !== undefined) {
		const read = readElement(record.value["title"], "$.head.title");
		if (read.kind === "error") {
			return read;
		}
		title = read.value;
	}
	const meta: Array<HeadEl> = [];
	if (record.value["meta"] !== undefined) {
		if (!Array.isArray(record.value["meta"])) {
			return failRead({ path: "$.head.meta", expected: "an array" });
		}
		for (const [index, entry] of record.value["meta"].entries()) {
			const read = readElement(entry, `$.head.meta[${index}]`);
			if (read.kind === "error") {
				return read;
			}
			meta.push(read.value);
		}
	}
	const rest: Array<HeadEl | HeadRawHtml> = [];
	if (record.value["rest"] !== undefined) {
		if (!Array.isArray(record.value["rest"])) {
			return failRead({ path: "$.head.rest", expected: "an array" });
		}
		for (const [index, entry] of record.value["rest"].entries()) {
			const path = `$.head.rest[${index}]`;
			if (
				typeof entry === "object" &&
				entry !== null &&
				"raw_html" in entry
			) {
				if (typeof entry.raw_html !== "string") {
					return failRead({
						path: `${path}.raw_html`,
						expected: "a string",
					});
				}
				const key = "key" in entry ? entry.key : undefined;
				if (typeof key !== "string" || !checkRawHtmlKey(key)) {
					return failRead({
						path: `${path}.key`,
						expected: "32 lowercase hexadecimal characters",
					});
				}
				rest.push({ rawHtml: entry.raw_html, key });
				continue;
			}
			const read = readElement(entry, path);
			if (read.kind === "error") {
				return read;
			}
			rest.push(read.value);
		}
	}
	return createSuccessResult({ title, meta, rest });
}

function readDocumentAttrs(
	value: unknown,
): Result<DocumentAttrs, PayloadReadError> {
	if (value === undefined) {
		return createSuccessResult({ html: {}, head: {}, body: {} });
	}
	const record = readRecord(value, "$.document_attrs");
	if (record.kind === "error") {
		return record;
	}
	const sections = {} as Record<
		"html" | "head" | "body",
		Readonly<Record<string, string>>
	>;
	for (const name of ["html", "head", "body"] as const) {
		const read =
			record.value[name] === undefined
				? createSuccessResult({} as Readonly<Record<string, string>>)
				: readStringRecord(
						record.value[name],
						`$.document_attrs.${name}`,
					);
		if (read.kind === "error") {
			return read;
		}
		sections[name] = read.value;
	}
	return createSuccessResult(sections);
}

/**
 * Reads the static public assets that a page preloads: a list, in the page's
 * order, of records with a URL and a request destination. The server writes
 * each URL once, so a URL that an earlier record names is an error.
 */
function readStaticPublicAssetPreloads(
	value: unknown,
): Result<ReadonlyArray<StaticPublicAssetPreload>, PayloadReadError> {
	if (value === undefined) {
		return createSuccessResult([]);
	}
	const path = "$.static_public_asset_preloads";
	if (!Array.isArray(value)) {
		return failRead({ path, expected: "an array" });
	}
	const preloads: Array<StaticPublicAssetPreload> = [];
	const urls = new Set<string>();
	for (const [index, entry] of value.entries()) {
		const entryPath = `${path}[${index}]`;
		const record = readRecord(entry, entryPath);
		if (record.kind === "error") {
			return record;
		}
		const { url, destination } = record.value;
		if (typeof url !== "string" || urls.has(url)) {
			return failRead({
				path: `${entryPath}.url`,
				expected: "a string that no earlier preload names",
			});
		}
		if (destination !== "image" && destination !== "font") {
			return failRead({
				path: `${entryPath}.destination`,
				expected: '"image" or "font"',
			});
		}
		urls.add(url);
		preloads.push({ url, destination });
	}
	return createSuccessResult(preloads);
}

/** Reads a navigation payload. */
export function readNavPayload(
	raw: unknown,
): Result<NavPayload, PayloadReadError> {
	const record = readRecord(raw, "$");
	if (record.kind === "error") {
		return record;
	}
	const routePayload = readPagePayload(record.value);
	if (routePayload.kind === "error") {
		return routePayload;
	}
	const head = readHead(record.value["head"]);
	if (head.kind === "error") {
		return head;
	}
	const documentAttrs = readDocumentAttrs(record.value["document_attrs"]);
	if (documentAttrs.kind === "error") {
		return documentAttrs;
	}
	const modulePreloadUrls = readOptionalStringArray(record.value, "deps");
	if (modulePreloadUrls.kind === "error") {
		return modulePreloadUrls;
	}
	const cssBundles = readOptionalStringArray(record.value, "css_bundles");
	if (cssBundles.kind === "error") {
		return cssBundles;
	}
	const staticPublicAssetPreloads = readStaticPublicAssetPreloads(
		record.value["static_public_asset_preloads"],
	);
	if (staticPublicAssetPreloads.kind === "error") {
		return staticPublicAssetPreloads;
	}
	return createSuccessResult({
		...routePayload.value,
		head: head.value,
		documentAttrs: documentAttrs.value,
		modulePreloadUrls: modulePreloadUrls.value,
		cssBundles: cssBundles.value,
		staticPublicAssetPreloads: staticPublicAssetPreloads.value,
	});
}

/** Reads the initial document's payload. */
export function readBootstrapPayload(
	raw: unknown,
): Result<BootstrapPayload, PayloadReadError> {
	const record = readRecord(raw, "$");
	if (record.kind === "error") {
		return record;
	}
	const routePayload = readPagePayload(record.value);
	if (routePayload.kind === "error") {
		return routePayload;
	}
	const clientBuildId = record.value["client_build_id"];
	if (typeof clientBuildId !== "string") {
		return failRead({ path: "$.client_build_id", expected: "a string" });
	}
	const documentAttrs = readDocumentAttrs(record.value["document_attrs"]);
	if (documentAttrs.kind === "error") {
		return documentAttrs;
	}
	const deploymentId = record.value["deployment_id"];
	if (deploymentId !== undefined && typeof deploymentId !== "string") {
		return failRead({ path: "$.deployment_id", expected: "a string" });
	}
	// The server writes `is_dev` only for a development document.
	const isDev = record.value["is_dev"];
	if (isDev !== undefined && typeof isDev !== "boolean") {
		return failRead({ path: "$.is_dev", expected: "a boolean" });
	}
	return createSuccessResult({
		...routePayload.value,
		clientBuildId,
		documentAttrs: documentAttrs.value,
		devMode: isDev === true,
		deploymentId,
	});
}

/** An endpoint's failure envelope: `{ failure: { status, declared_error | rejection } }`. */
export function readEndpointFailureEnvelope(
	raw: unknown,
): Result<ServerFailure, PayloadReadError> {
	const record = readRecord(raw, "$");
	if (record.kind === "error") {
		return record;
	}
	const failure = readRecord(record.value["failure"], "$.failure");
	if (failure.kind === "error") {
		return failure;
	}
	return readFailureFields(failure.value, "$.failure");
}
