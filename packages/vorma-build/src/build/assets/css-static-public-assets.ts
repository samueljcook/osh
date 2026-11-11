import { Result } from "effect";
import {
	isCSSRequest,
	type Plugin,
	type RenderBuiltAssetUrl,
	type Rolldown,
	type UserConfig,
} from "vite";
import type { StaticPublicAssetProjection } from "./static-public-assets.ts";

export type CssStaticPublicAssets = {
	readonly jsAssets: ReadonlyMap<string, ReadonlyArray<JsAssetReference>>;
	/** The plugins that resolve `@public/` URLs and record the assets that scripts reference. */
	readonly plugins: ReadonlyArray<Plugin>;
	/** The plugins that resolve `@public/` URLs in a worker bundle. */
	readonly workerPlugins: ReadonlyArray<Plugin>;
	readonly updateStaticPublicAssets: (
		staticPublicAssets: Pick<
			StaticPublicAssetProjection,
			"staticPublicAssetUrls"
		>,
	) => void;
};

/** Native URL-hook metadata for nonblocking resource hints. */
export type JsAssetReference = {
	readonly fileName: string;
	readonly renderedUrl: ReturnType<RenderBuiltAssetUrl>;
};

// Each match is a quoted string, a comment, a url() token, the start of an
// image-set() function, or a parenthesis. Quoted URLs may contain spaces and
// parentheses, and escaped characters do not terminate their token.
const CSS_URL_PATTERN =
	/("(?:\\[\s\S]|[^"\\])*"|'(?:\\[\s\S]|[^'\\])*')|\/\*[\s\S]*?\*\/|\burl\(\s*(?:"((?:\\[\s\S]|[^"\\])*)"|'((?:\\[\s\S]|[^'\\])*)'|((?:\\[\s\S]|[^)\\])*?))\s*\)|((?<![\w-])(?:-webkit-)?image-set\()|([()])/gi;

const CSS_COMMENT_PATTERN = /\/\*[\s\S]*?\*\//g;

const PUBLIC_URL_PREFIX = "@public/";

/** The requests for a stylesheet file with a module that is not CSS text: raw, URL, and worker imports. */
const NON_CSS_TEXT_QUERY = /[?&](?:worker|sharedworker|raw|url)\b/;

/**
 * Adds the `@public/` rule to an `external` setting. Vite reads the resolved
 * `external` setting to decide whether to warn that a stylesheet's URL did not
 * resolve at build time, and Vorma replaces each `@public/` URL that it
 * accepts before the build resolves it.
 */
function addPublicUrlExternal(
	external: Rolldown.RolldownOptions["external"],
): NonNullable<Rolldown.RolldownOptions["external"]> {
	if (typeof external === "function") {
		return (source, importer, isResolved) => {
			return (
				source.startsWith(PUBLIC_URL_PREFIX) ||
				external(source, importer, isResolved)
			);
		};
	}
	return [
		...(external === undefined
			? []
			: Array.isArray(external)
				? external
				: [external]),
		new RegExp(`^${PUBLIC_URL_PREFIX}`),
	];
}

/** The kind of each open parenthesis around a position in a stylesheet. */
type CssParenthesis = "image-set" | "other";

/**
 * Resolves `@public/` URLs in the stylesheets that Vite compiles, after
 * Vite's own CSS processing, so that the application's PostCSS configuration
 * and its CSS transformer run as they do without Vorma. A static public asset
 * reference is `@public/` followed by the asset's key, the exact path of the
 * asset relative to `core.staticPublicAssets.srcDir`, with URL percent encoding
 * for `%`, `?`, and `#` in file names, and optionally a query
 * and a fragment, written as the whole URL of a `url()` or as a string in
 * `image-set()` or `-webkit-image-set()`. A reference to a key that is not
 * the key of an asset, and any other `@public/` outside a comment, fails the
 * build. Vite rewrites a `url()` of a Sass file that a file in another
 * directory imports relative to the importing file, so a reference there
 * keeps its `@public/` form only in an unquoted Sass variable, as in
 * `url($logo)`. The replacement runs before Vite extracts and hashes
 * the stylesheet, so the stylesheet's file name follows the hashed URLs that
 * it contains. Static public assets are already published.
 */
export function createCssStaticPublicAssets(
	staticPublicAssets: Pick<
		StaticPublicAssetProjection,
		"staticPublicAssetUrls"
	>,
): CssStaticPublicAssets {
	const jsAssets = new Map<string, Array<JsAssetReference>>();
	/** The CSS text that replaces a reference, or why the reference has no replacement. */
	function replaceReference(
		reference: string,
	): Result.Result<string, string> {
		const suffixIndex = reference.search(/[?#]/);
		const keyEnd = suffixIndex < 0 ? reference.length : suffixIndex;
		const decodedKey = Result.try(() => {
			return decodeURIComponent(
				reference.slice(PUBLIC_URL_PREFIX.length, keyEnd),
			);
		});
		if (Result.isFailure(decodedKey)) {
			return Result.fail(
				`Invalid percent encoding in static public asset URL: ${reference}`,
			);
		}
		const key = decodedKey.success;
		const assetUrl = Object.hasOwn(
			staticPublicAssets.staticPublicAssetUrls,
			key,
		)
			? staticPublicAssets.staticPublicAssetUrls[key]
			: undefined;
		if (assetUrl === undefined) {
			return Result.fail(
				`Unknown static public asset: ${reference} names the key ${JSON.stringify(key)}, which is not the path of a file in core.staticPublicAssets.srcDir relative to that directory`,
			);
		}
		return Result.succeed(
			'"' +
				(assetUrl + reference.slice(keyEnd)).replace(
					/["\\\n\r\f]/g,
					(character) =>
						"\\" + character.charCodeAt(0).toString(16) + " ",
				) +
				'"',
		);
	}
	function replaceUrls(css: string): Result.Result<string, string> {
		const errors: Array<string> = [];
		const parentheses: Array<CssParenthesis> = [];
		/**
		 * The stylesheet's text without the references that the replacement
		 * replaced, which the scan for a remaining `@public/` reads, so that a
		 * serve prefix that contains `@public/` does not count as one.
		 */
		const unreplacedParts: Array<string> = [];
		let unreplacedEnd = 0;
		function recordReplacement(input: {
			readonly offset: number;
			readonly length: number;
		}) {
			unreplacedParts.push(css.slice(unreplacedEnd, input.offset));
			unreplacedEnd = input.offset + input.length;
		}
		const replaced = css.replace(
			CSS_URL_PATTERN,
			(
				matched,
				quotedString: string | undefined,
				doubleQuoted: string | undefined,
				singleQuoted: string | undefined,
				unquoted: string | undefined,
				imageSet: string | undefined,
				parenthesis: string | undefined,
				offset: number,
			) => {
				if (imageSet !== undefined) {
					parentheses.push("image-set");
					return matched;
				}
				if (parenthesis !== undefined) {
					if (parenthesis === "(") {
						parentheses.push("other");
					} else {
						parentheses.pop();
					}
					return matched;
				}
				const token =
					quotedString !== undefined
						? parentheses.at(-1) === "image-set"
							? quotedString.slice(1, -1)
							: undefined
						: (doubleQuoted ?? singleQuoted ?? unquoted);
				if (token === undefined) {
					return matched;
				}
				const reference = decodeCssEscapes(token);
				if (!reference.startsWith(PUBLIC_URL_PREFIX)) {
					return matched;
				}
				const replacement = replaceReference(reference);
				if (Result.isFailure(replacement)) {
					errors.push(replacement.failure);
					return matched;
				}
				recordReplacement({ offset, length: matched.length });
				return quotedString === undefined
					? `url(${replacement.success})`
					: replacement.success;
			},
		);
		if (errors.length > 0) {
			return Result.fail(errors.join("\n"));
		}
		unreplacedParts.push(css.slice(unreplacedEnd));
		const unreplaced = unreplacedParts
			.join("")
			.replace(CSS_COMMENT_PATTERN, "")
			.match(/[^\s;{}]*@public\/[^\s;{}]*/);
		return unreplaced === null
			? Result.succeed(replaced)
			: Result.fail(
					`The stylesheet contains ${JSON.stringify(unreplaced[0])}, a static public asset reference that Vorma does not replace. Write each reference as the whole URL of a url(), such as url("@public/logo.svg"), or as a string in image-set(). Vite rewrites the url() arguments of a Sass file that a file in another directory imports, except an unquoted Sass variable, so in such a file write url($logo) with $logo: "@public/logo.svg".`,
				);
	}
	const urlPlugin: Plugin = {
		name: "vorma:css-public-urls",
		configResolved(config) {
			// A worker's configuration shares the build options of the
			// configuration that it belongs to, which already has the rule.
			if (config.isWorker) {
				return;
			}
			config.build.rolldownOptions.external = addPublicUrlExternal(
				config.build.rolldownOptions.external,
			);
		},
		transform(css, id) {
			if (!isCSSRequest(id) || NON_CSS_TEXT_QUERY.test(id)) {
				return null;
			}
			const replaced = replaceUrls(css);
			return Result.isFailure(replaced)
				? this.error(`${id}: ${replaced.failure}`)
				: { code: replaced.success, map: null };
		},
	};
	const jsAssetReferencePlugin: Plugin = {
		name: "vorma:js-asset-references",
		enforce: "post",
		config(config) {
			const renderBuiltUrl = config.experimental?.renderBuiltUrl;
			return {
				experimental: {
					renderBuiltUrl(fileName, ctx) {
						const renderedUrl = renderBuiltUrl?.(fileName, ctx);
						if (ctx.hostType === "js") {
							const references = jsAssets.get(ctx.hostId);
							const reference = { fileName, renderedUrl };
							if (references === undefined) {
								jsAssets.set(ctx.hostId, [reference]);
							} else {
								references.push(reference);
							}
						}
						return renderedUrl;
					},
				},
			} satisfies UserConfig;
		},
	};
	return {
		jsAssets,
		plugins: [urlPlugin, jsAssetReferencePlugin],
		workerPlugins: [urlPlugin],
		updateStaticPublicAssets(next) {
			staticPublicAssets = next;
		},
	};
}

/** Decodes escapes inside an already identified CSS URL token. */
function decodeCssEscapes(value: string): string {
	return value.replace(
		/\\(?:([0-9a-f]{1,6})(?:\r\n|[ \t\n\r\f])?|(\r\n|[\n\r\f])|([\s\S]))/gi,
		(
			_match,
			hex: string | undefined,
			newline: string | undefined,
			escaped: string | undefined,
		) => {
			if (hex !== undefined) {
				const codePoint = Number.parseInt(hex, 16);
				return codePoint === 0 ||
					codePoint > 0x10ffff ||
					(codePoint >= 0xd800 && codePoint <= 0xdfff)
					? "\ufffd"
					: String.fromCodePoint(codePoint);
			}
			return newline === undefined ? (escaped ?? "") : "";
		},
	);
}
