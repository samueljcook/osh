import { getErrorMsg } from "primitives-ts/error-msgs";
import {
	createErrorResult,
	createSuccessResult,
	callWithResult,
	type Result,
} from "primitives-ts/results";
import { DOCUMENT_PROTOCOL } from "../protocol/constants.ts";
import {
	checkRawHtmlKey,
	type HeadEl,
	type HeadRawHtml,
	type HeadSections,
} from "../protocol/wire.ts";

/*
Reconciles the three managed head regions. The server bounds each region with a
`<meta name="vorma-head" content="...">` element: a region is the nodes between
its marker and the next marker. The regions belong to Vorma: reconciliation
removes every node of a region that the destination's head does not contain,
including a node that other code inserted there. The client never removes a
marker and never writes the `assets` region.

An element item keeps its DOM node while the destination has an element with
the same tag, attributes, and inner HTML (its fingerprint); among the
destination's elements, a later element with a fingerprint replaces an earlier
one and takes the later position.

A raw HTML item has a key of 32 lowercase hexadecimal characters, which the
server computes and the client never computes. In a document, the item is the
comment `<!--vorma-raw:KEY-->`, the item's nodes, and the comment
`<!--/vorma-raw:KEY-->`: the server writes the comments into the document that
it renders, and the client writes them around the nodes of an item that it
inserts. The two comments and every node between them are one unit with that
key, including nodes that the item's own scripts insert there. Deduplication
does not apply to raw HTML items, and items with the same key are matched in
order. A unit with a key that the region and the destination both have keeps
its nodes, so its scripts do not run again; a raw HTML item that the
destination adds is parsed as a fragment and inserted, and its scripts run; a
unit that the destination does not have is removed with its comments.

Each region ends in server order. Reconciliation creates the nodes of every
region before it changes any region, so an item that cannot be created changes
nothing.
*/

export type HeadReconciliationError = {
	readonly kind: "head-reconciliation-error";
	readonly msg: string;
};

const MARKER_NAME = DOCUMENT_PROTOCOL.headMarkers.name;
const REGIONS = DOCUMENT_PROTOCOL.headMarkers.regions;

type Region = {
	readonly selectFn: (
		sections: HeadSections,
	) => ReadonlyArray<HeadEl | HeadRawHtml>;
	readonly start: string;
	readonly end: string;
};

const TITLE_REGION: Region = {
	selectFn: (sections) =>
		sections.title === undefined ? [] : [sections.title],
	start: REGIONS.title,
	end: REGIONS.meta,
};

const META_REGION: Region = {
	selectFn: (sections) => sections.meta,
	start: REGIONS.meta,
	end: REGIONS.rest,
};

const REST_REGION: Region = {
	selectFn: (sections) => sections.rest,
	start: REGIONS.rest,
	end: REGIONS.assets,
};

const MANAGED_REGIONS: ReadonlyArray<Region> = [
	TITLE_REGION,
	META_REGION,
	REST_REGION,
];

function failHead(msg: string): Result<never, HeadReconciliationError> {
	return createErrorResult({ kind: "head-reconciliation-error", msg });
}

type Boundary = { readonly start: Element; readonly end: Element };

function findBoundary(region: Region): Boundary | null {
	let start: Element | undefined;
	let end: Element | undefined;
	for (const marker of document.head.querySelectorAll(
		`meta[name="${MARKER_NAME}"]`,
	)) {
		const content = marker.getAttribute("content");
		if (content === region.start && start === undefined) {
			start = marker;
		} else if (content === region.end && end === undefined) {
			end = marker;
		}
	}
	if (start === undefined || end === undefined) {
		return null;
	}
	return start.parentNode === end.parentNode ? { start, end } : null;
}

function readRegionNodes(boundary: Boundary): Array<Node> {
	const nodes: Array<Node> = [];
	for (
		let node = boundary.start.nextSibling;
		node !== null && node !== boundary.end;
		node = node.nextSibling
	) {
		nodes.push(node);
	}
	return nodes;
}

/**
 * Applies the sections to the document's regions. A missing marker, or an
 * element item that cannot be created, fails before any region changes.
 */
export function applyHead(
	sections: HeadSections,
): Result<void, HeadReconciliationError> {
	const boundaries: Array<{ region: Region; boundary: Boundary }> = [];
	for (const region of MANAGED_REGIONS) {
		const boundary = findBoundary(region);
		if (boundary === null) {
			if (region.selectFn(sections).length === 0) {
				continue;
			}
			return failHead(`Missing head marker ${region.start}`);
		}
		boundaries.push({ region, boundary });
	}
	const plans: Array<RegionPlan> = [];
	for (const { region, boundary } of boundaries) {
		const plan = callWithResult(() =>
			planRegion({ items: region.selectFn(sections), boundary }),
		);
		if (plan.kind === "error") {
			return failHead(
				getErrorMsg(plan.error) ??
					"The document head could not be updated.",
			);
		}
		plans.push(plan.value);
	}
	for (const plan of plans) {
		// A raw HTML script runs as it is inserted and can change the head.
		const applied = callWithResult(() => applyRegionPlan(plan));
		if (applied.kind === "error") {
			return failHead(
				getErrorMsg(applied.error) ??
					"The document head could not be updated.",
			);
		}
	}
	return createSuccessResult(undefined);
}

/**
 * Reads the server-rendered head state from the three managed regions. The
 * title and meta regions hold elements; the rest region also holds raw HTML
 * units.
 */
export function readDocumentHead(): Result<
	HeadSections,
	HeadReconciliationError
> {
	const title = findBoundary(TITLE_REGION);
	const meta = findBoundary(META_REGION);
	const rest = findBoundary(REST_REGION);
	if (title === null || meta === null || rest === null) {
		const missing =
			title === null
				? TITLE_REGION
				: meta === null
					? META_REGION
					: REST_REGION;
		return failHead(`Missing head marker ${missing.start}`);
	}
	const titleEls = readRegionEls(title);
	if (titleEls.length > 1) {
		return failHead(
			"The managed title region contains more than one element",
		);
	}
	return createSuccessResult({
		title: titleEls[0],
		meta: readRegionEls(meta),
		rest: readRegionParts(rest).flatMap(
			(part): Array<HeadEl | HeadRawHtml> => {
				return part.kind === "raw-html"
					? [readRawHtmlItem(part.unit)]
					: part.node.nodeType === Node.ELEMENT_NODE
						? [readHeadEl(part.node as Element)]
						: [];
			},
		),
	});
}

function readRegionEls(boundary: Boundary): Array<HeadEl> {
	return readRegionNodes(boundary).flatMap((node) => {
		return node.nodeType === Node.ELEMENT_NODE
			? [readHeadEl(node as Element)]
			: [];
	});
}

/** The element of one element item. */
function createHeadEl(wire: HeadEl): Element {
	const element = document.createElement(wire.tag);
	if (wire.attrs !== undefined) {
		for (const [name, value] of Object.entries(wire.attrs)) {
			element.setAttribute(name, value);
		}
	}
	if (wire.html !== undefined) {
		element.innerHTML = wire.html;
	}
	return element;
}

function readHeadEl(element: Element): HeadEl {
	const attrs = Object.fromEntries(
		[...element.attributes].map((attr) => {
			return [attr.name, attr.value];
		}),
	);
	return {
		tag: element.localName,
		...(Object.keys(attrs).length === 0 ? {} : { attrs }),
		...(element.innerHTML === "" ? {} : { html: element.innerHTML }),
	};
}

function computeFingerprint(element: Element): string {
	const attrs: Array<readonly [string, string]> = [];
	for (const attr of element.attributes) {
		attrs.push([attr.name, attr.value]);
	}
	attrs.sort(([left], [right]) => (left < right ? -1 : left > right ? 1 : 0));
	return JSON.stringify([
		element.tagName.toUpperCase(),
		attrs,
		element.innerHTML,
	]);
}

const RAW_HTML_MARKERS = DOCUMENT_PROTOCOL.rawHtmlMarkers;

/** The nodes of one raw HTML item in a region: its opening comment, its own nodes, and its closing comment. */
type RawHtmlUnit = {
	readonly key: string;
	readonly nodes: ReadonlyArray<Node>;
};

/** A part of a region's content: a raw HTML unit, or any other node. */
type RegionPart =
	| { readonly kind: "raw-html"; readonly unit: RawHtmlUnit }
	| { readonly kind: "node"; readonly node: Node };

/** The key of a raw HTML marker comment with the prefix, or undefined for any other node. */
function readMarkerKey(input: {
	readonly node: Node;
	readonly prefix: string;
}): string | undefined {
	const { node, prefix } = input;
	if (node.nodeType !== Node.COMMENT_NODE) {
		return undefined;
	}
	const data = (node as Comment).data;
	if (!data.startsWith(prefix)) {
		return undefined;
	}
	const key = data.slice(prefix.length);
	return checkRawHtmlKey(key) ? key : undefined;
}

/**
 * Splits a region's nodes into raw HTML units and other nodes, in document
 * order. A unit runs from an opening comment to the next closing comment with
 * the same key; an opening comment without one is an ordinary node.
 */
function readRegionParts(boundary: Boundary): Array<RegionPart> {
	const nodes = readRegionNodes(boundary);
	const parts: Array<RegionPart> = [];
	let index = 0;
	for (;;) {
		const node = nodes[index];
		if (node === undefined) {
			return parts;
		}
		const key = readMarkerKey({ node, prefix: RAW_HTML_MARKERS.start });
		const end =
			key === undefined
				? -1
				: nodes.findIndex((candidate, position) => {
						return (
							position > index &&
							readMarkerKey({
								node: candidate,
								prefix: RAW_HTML_MARKERS.end,
							}) === key
						);
					});
		if (key === undefined || end === -1) {
			parts.push({ kind: "node", node });
			index += 1;
		} else {
			parts.push({
				kind: "raw-html",
				unit: { key, nodes: nodes.slice(index, end + 1) },
			});
			index = end + 1;
		}
	}
}

/** The raw HTML item of a unit in the document, with the markup of the nodes between its comments. */
function readRawHtmlItem(unit: RawHtmlUnit): HeadRawHtml {
	const template = document.createElement("template");
	template.content.append(
		...unit.nodes.slice(1, -1).map((node) => node.cloneNode(true)),
	);
	return { rawHtml: template.innerHTML, key: unit.key };
}

/**
 * Creates the unit of a raw HTML item that the region does not hold: the
 * item's nodes, parsed as a fragment, between its two comments. The scripts of
 * a fragment run when the client inserts the fragment's nodes into the
 * document.
 */
function createRawHtmlUnit(item: HeadRawHtml): RawHtmlUnit {
	const range = document.createRange();
	range.selectNodeContents(document.head);
	return {
		key: item.key,
		nodes: [
			document.createComment(`${RAW_HTML_MARKERS.start}${item.key}`),
			...range.createContextualFragment(item.rawHtml).childNodes,
			document.createComment(`${RAW_HTML_MARKERS.end}${item.key}`),
		],
	};
}

/** The nodes that a region holds after reconciliation, in order. */
type RegionPlan = {
	readonly boundary: Boundary;
	readonly nodes: ReadonlyArray<Node>;
};

type PlannedItem =
	| {
			readonly kind: "element";
			readonly element: Element;
			readonly fingerprint: string;
	  }
	| { readonly kind: "raw-html"; readonly unit: RawHtmlUnit };

/** Decides a region's nodes without changing the document. */
function planRegion(input: {
	readonly items: ReadonlyArray<HeadEl | HeadRawHtml>;
	readonly boundary: Boundary;
}): RegionPlan {
	const unitsByKey = new Map<string, Array<RawHtmlUnit>>();
	const elementsByFingerprint = new Map<string, Array<Element>>();
	for (const part of readRegionParts(input.boundary)) {
		if (part.kind === "raw-html") {
			const units = unitsByKey.get(part.unit.key);
			if (units === undefined) {
				unitsByKey.set(part.unit.key, [part.unit]);
			} else {
				units.push(part.unit);
			}
		} else if (part.node.nodeType === Node.ELEMENT_NODE) {
			const element = part.node as Element;
			const fingerprint = computeFingerprint(element);
			const elements = elementsByFingerprint.get(fingerprint);
			if (elements === undefined) {
				elementsByFingerprint.set(fingerprint, [element]);
			} else {
				elements.push(element);
			}
		}
	}
	const usedUnits = new Set<RawHtmlUnit>();
	function resolveRawHtmlUnit(item: HeadRawHtml): RawHtmlUnit {
		const held = unitsByKey
			.get(item.key)
			?.find((unit) => !usedUnits.has(unit));
		if (held === undefined) {
			return createRawHtmlUnit(item);
		}
		usedUnits.add(held);
		return held;
	}
	const created = input.items.map((item) => {
		if ("rawHtml" in item) {
			return item;
		}
		const element = createHeadEl(item);
		return { element, fingerprint: computeFingerprint(element) };
	});
	const lastPositionByFingerprint = new Map<string, number>();
	for (const [position, item] of created.entries()) {
		if (!("rawHtml" in item)) {
			lastPositionByFingerprint.set(item.fingerprint, position);
		}
	}
	const planned: Array<PlannedItem> = [];
	for (const [position, item] of created.entries()) {
		if ("rawHtml" in item) {
			planned.push({ kind: "raw-html", unit: resolveRawHtmlUnit(item) });
		} else if (
			lastPositionByFingerprint.get(item.fingerprint) === position
		) {
			planned.push({ kind: "element", ...item });
		}
	}
	const usedElements = new Set<Element>();
	const nodes: Array<Node> = [];
	for (const item of planned) {
		if (item.kind === "raw-html") {
			nodes.push(...item.unit.nodes);
			continue;
		}
		const match = elementsByFingerprint
			.get(item.fingerprint)
			?.find((element) => !usedElements.has(element));
		if (match === undefined) {
			nodes.push(item.element);
		} else {
			usedElements.add(match);
			nodes.push(match);
		}
	}
	return { boundary: input.boundary, nodes };
}

/** Changes a region to hold the plan's nodes in order, moving a node only when it is out of place. */
function applyRegionPlan(plan: RegionPlan): void {
	const { boundary } = plan;
	const parent = boundary.end.parentNode;
	if (parent === null) {
		throw new Error("Detached head marker");
	}
	const kept = new Set(plan.nodes);
	for (const node of readRegionNodes(boundary)) {
		if (!kept.has(node)) {
			parent.removeChild(node);
		}
	}
	let cursor: Node = boundary.start;
	for (const node of plan.nodes) {
		if (cursor.nextSibling !== node) {
			parent.insertBefore(node, cursor.nextSibling);
		}
		cursor = node;
	}
}
