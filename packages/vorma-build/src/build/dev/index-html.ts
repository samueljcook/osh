import { Result } from "effect";

/*
Vite's `transformIndexHtml` output is written for a page that Vite serves, so
its URLs are relative to the page's own origin. A Vorma document comes from the
application's server, and every script, module preload, and stylesheet that
Vite's plugins add to its head or body loads from the module server. Other
URLs, such as an icon's, name files that the application serves.
*/

const ROOT_RELATIVE_URL = String.raw`/(?!/)[^"'\s]*`;

/** A script element, with its attributes and its content. */
const SCRIPT_ELEMENT_PATTERN = /<script\b([^>]*)>([\s\S]*?)<\/script\s*>/gi;

/** A link element's start tag. */
const LINK_TAG_PATTERN = /<link\b[^>]*>/gi;

/** A `src` attribute with a root-relative URL value. */
const SRC_ATTR_PATTERN = new RegExp(
	String.raw`(\bsrc\s*=\s*)(["'])(${ROOT_RELATIVE_URL})\2`,
	"gi",
);

/** An `href` attribute with a root-relative URL value. */
const HREF_ATTR_PATTERN = new RegExp(
	String.raw`(\bhref\s*=\s*)(["'])(${ROOT_RELATIVE_URL})\2`,
	"gi",
);

/** A `rel` attribute's value. */
const REL_ATTR_PATTERN = /\brel\s*=\s*(["'])([^"']*)\1/i;

/** A static, dynamic, or side-effect import of a root-relative specifier in an inline module. */
const IMPORT_PATTERN = new RegExp(
	String.raw`(\b(?:from|import)\s*\(?\s*)(["'])(${ROOT_RELATIVE_URL})\2`,
	"g",
);

/** The link relations with URLs that name a module or a stylesheet that the module server serves. */
const MODULE_SERVER_LINK_RELATIONS = new Set(["modulepreload", "stylesheet"]);

/**
 * The markup that Vite produced, with the root-relative URL of every script's
 * `src`, of every module preload's and stylesheet's `href`, and of every import
 * specifier of an inline script resolved against the module server's origin.
 * Absolute and protocol-relative URLs, and the URLs of other elements, stay as
 * they are.
 */
export function toModuleServerHtml(input: {
	readonly html: string;
	/** `http://127.0.0.1:<port>`. */
	readonly moduleServerOrigin: string;
}): string {
	function resolve(
		_match: string,
		prefix: string,
		quote: string,
		url: string,
	) {
		return `${prefix}${quote}${input.moduleServerOrigin}${url}${quote}`;
	}
	return input.html
		.replace(SCRIPT_ELEMENT_PATTERN, (_element, attrs, content) => {
			const resolvedAttrs = String(attrs).replace(
				SRC_ATTR_PATTERN,
				resolve,
			);
			const resolvedContent = String(content).replace(
				IMPORT_PATTERN,
				resolve,
			);
			return `<script${resolvedAttrs}>${resolvedContent}</script>`;
		})
		.replace(LINK_TAG_PATTERN, (tag) => {
			const relations = (REL_ATTR_PATTERN.exec(tag)?.[2] ?? "")
				.toLowerCase()
				.split(/\s+/);
			return relations.some((relation) => {
				return MODULE_SERVER_LINK_RELATIONS.has(relation);
			})
				? tag.replace(HREF_ATTR_PATTERN, resolve)
				: tag;
		});
}

/** What Vite's plugins add to a document, by the place where each part goes. */
export type IndexHtmlParts = {
	/** The tags with `injectTo` set to `"head-prepend"`, `"head"`, or nothing, in Vite's order. */
	readonly headHtml: string;
	/** The tags with `injectTo` set to `"body-prepend"`. */
	readonly bodyPrependHtml: string;
	/** The tags with `injectTo` set to `"body"`. */
	readonly bodyHtml: string;
};

const BODY_SEPARATOR = "<!--vorma-body-separator-->";

/**
 * The page that `splitIndexHtml` expects Vite to transform. The comment in
 * the body separates the tags that Vite puts at the start of the body from the
 * tags that it puts at the end.
 */
export const INDEX_HTML_TEMPLATE = `<head></head><body>${BODY_SEPARATOR}</body>`;

/**
 * Splits Vite's transformation of `INDEX_HTML_TEMPLATE` into what goes in each
 * place of the document. It fails with a message when a plugin removed or
 * changed the template's `<head>`, `</head>`, `<body>`, or `</body>` tag or the
 * separator comment, because Vorma then cannot tell where the plugins' tags go.
 */
export function splitIndexHtml(
	html: string,
): Result.Result<IndexHtmlParts, string> {
	const headStart = html.indexOf("<head>");
	const headEnd = html.lastIndexOf("</head>");
	const bodyStart = html.indexOf("<body>");
	const bodyEnd = html.lastIndexOf("</body>");
	const markers = [
		{ name: "<head>", index: headStart },
		{ name: "</head>", index: headEnd },
		{ name: "<body>", index: bodyStart },
		{ name: "</body>", index: bodyEnd },
	];
	const missing = markers.find((marker) => marker.index === -1);
	if (missing !== undefined) {
		return Result.fail(
			`Vite's transformIndexHtml output has no ${missing.name} tag. A plugin of the Vite configuration removed or changed it; Vorma needs the tags that it gives Vite to place the plugins' tags in the document.`,
		);
	}
	if (!(headStart < headEnd && headEnd < bodyStart && bodyStart < bodyEnd)) {
		return Result.fail(
			"Vite's transformIndexHtml output does not have the <head> element before the <body> element. A plugin of the Vite configuration moved them; Vorma needs them in the order that it gives Vite.",
		);
	}
	const body = html.slice(bodyStart + "<body>".length, bodyEnd);
	const separatorStart = body.indexOf(BODY_SEPARATOR);
	if (separatorStart === -1) {
		return Result.fail(
			`Vite's transformIndexHtml output has no ${BODY_SEPARATOR} comment in the <body> element. A plugin of the Vite configuration removed it; Vorma needs it to tell the tags at the start of the body from the tags at its end.`,
		);
	}
	return Result.succeed({
		headHtml: html.slice(headStart + "<head>".length, headEnd),
		bodyPrependHtml: body.slice(0, separatorStart),
		bodyHtml: body.slice(separatorStart + BODY_SEPARATOR.length),
	});
}
