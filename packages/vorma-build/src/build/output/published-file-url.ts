import { Result } from "effect";
import type { StaticPublicAssetsServeUrlPathPrefix } from "../config/resolve-config.ts";

/*
Vite percent-encodes asset URLs with encodeURI. Native chunk and source-map
names use portable URL-safe stems, so native imports and metadata agree with
Vite's asset URLs. The server decodes each request path before file lookup.
*/

// Vite treats literal question marks and hash signs as URL suffix delimiters.
// Generated names escape these characters; explicit plugin file names must
// also preserve the path/suffix distinction that Vite's URL hooks require.
const URL_PATH_UNSAFE_CHARACTER = /[?#\\\p{Cc}]/u;

const RENDERED_URL_UNSAFE_CHARACTER = /[\\"'`<>\s\p{Cc}]/u;

/**
 * The browser URL of a published file: the static public assets prefix
 * followed by the file name as `encodeURI` spells it. It fails, with a message
 * that names the file, for a name with a lone UTF-16 surrogate or a character
 * that a URL path cannot carry.
 */
export function toPublishedFileUrl(input: {
	readonly staticPublicAssetsServeUrlPathPrefix: StaticPublicAssetsServeUrlPathPrefix;
	readonly fileName: string;
}): Result.Result<string, string> {
	if (!input.fileName.isWellFormed()) {
		return Result.fail(
			`The build would publish ${JSON.stringify(input.fileName)}, which contains a lone UTF-16 surrogate that cannot be encoded in a URL. Rename the file that produces it, or remove the output option or plugin that names it.`,
		);
	}
	const unsafe = URL_PATH_UNSAFE_CHARACTER.exec(input.fileName);
	if (unsafe !== null) {
		return Result.fail(
			`The build would publish ${JSON.stringify(input.fileName)}, but a published file name cannot hold the character ${JSON.stringify(unsafe[0])}, which Vite treats as a URL suffix or which cannot occur in a published path. Rename the file that produces it, or remove the output option or plugin that names it.`,
		);
	}
	return Result.succeed(
		input.staticPublicAssetsServeUrlPathPrefix + encodeURI(input.fileName),
	);
}

/**
 * Checks a URL string that an application's `experimental.renderBuiltUrl`
 * returned and returns it unchanged, preserving existing percent-encoding,
 * query strings, and fragments. It fails for empty URLs, lone UTF-16
 * surrogates, disallowed URL schemes, missing URL authorities, `..` path
 * segments, and characters that an HTML attribute or preload URL cannot carry.
 */
export function toRenderedAssetUrl(
	renderedUrl: string,
): Result.Result<string, string> {
	if (renderedUrl.length === 0) {
		return Result.fail(
			"experimental.renderBuiltUrl returned an empty URL.",
		);
	}
	if (!renderedUrl.isWellFormed()) {
		return Result.fail(
			`experimental.renderBuiltUrl returned ${JSON.stringify(renderedUrl)}, which contains a lone UTF-16 surrogate that cannot appear in a URL.`,
		);
	}
	const unsafe = RENDERED_URL_UNSAFE_CHARACTER.exec(renderedUrl);
	if (unsafe !== null) {
		return Result.fail(
			`experimental.renderBuiltUrl returned ${JSON.stringify(renderedUrl)}, which contains the disallowed character ${JSON.stringify(unsafe[0])}; percent-encode reserved characters in the returned URL.`,
		);
	}
	const delimiterIndex = renderedUrl.search(/[/?#]/);
	const beforeDelimiter =
		delimiterIndex === -1
			? renderedUrl
			: renderedUrl.slice(0, delimiterIndex);
	if (beforeDelimiter.includes(":")) {
		const schemeSeparatorIndex = renderedUrl.indexOf(":");
		const scheme = renderedUrl.slice(0, schemeSeparatorIndex);
		if (!/^https?$/i.test(scheme)) {
			return Result.fail(
				`experimental.renderBuiltUrl returned ${JSON.stringify(renderedUrl)}, which uses an unsupported URL scheme; use a relative URL or an "http://" or "https://" URL.`,
			);
		}
		const afterScheme = renderedUrl.slice(schemeSeparatorIndex + 1);
		if (!testNonEmptyAuthority(afterScheme)) {
			return Result.fail(
				`experimental.renderBuiltUrl returned ${JSON.stringify(renderedUrl)}, which is missing a host after "${scheme}:".`,
			);
		}
	} else if (
		renderedUrl.startsWith("//") &&
		!testNonEmptyAuthority(renderedUrl)
	) {
		return Result.fail(
			`experimental.renderBuiltUrl returned ${JSON.stringify(renderedUrl)}, which is missing a host after "//".`,
		);
	}
	const suffixStart = renderedUrl.search(/[?#]/);
	const pathPart =
		suffixStart === -1 ? renderedUrl : renderedUrl.slice(0, suffixStart);
	if (pathPart.split("/").includes("..")) {
		return Result.fail(
			`experimental.renderBuiltUrl returned ${JSON.stringify(renderedUrl)}, which contains a ".." path segment.`,
		);
	}
	return Result.succeed(renderedUrl);
}

function testNonEmptyAuthority(afterColon: string): boolean {
	if (!afterColon.startsWith("//")) {
		return false;
	}
	const rest = afterColon.slice(2);
	const authorityEnd = rest.search(/[/?#]/);
	const authority = authorityEnd === -1 ? rest : rest.slice(0, authorityEnd);
	return authority.length > 0;
}
