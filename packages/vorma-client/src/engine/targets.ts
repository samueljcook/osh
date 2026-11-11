import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "primitives-ts/results";
import type { RawTarget } from "../generated-types.ts";
import { buildHref } from "../protocol/href.ts";
import { getCurrentCtx } from "./current.ts";

/** Whether the URL is a `javascript:` URL, which runs script when a browser follows it, so that no target accepts it. */
export function checkJavascriptUrl(url: URL): boolean {
	return url.protocol === "javascript:";
}

/**
 * Turns a target into an origin-relative href, or an error message. A raw
 * href is used as written, except that a `javascript:` URL is an error. Whether
 * Vercel skew protection is on comes from the current client, and it is off
 * when no client exists.
 */
export function resolveTargetHref(target: RawTarget): Result<string, string> {
	if (target.href !== undefined) {
		const url = callWithResult(
			() => new URL(target.href, window.location.href),
		);
		return url.kind === "ok" && checkJavascriptUrl(url.value)
			? createErrorResult("a target does not accept a javascript: URL")
			: createSuccessResult(target.href);
	}
	if (typeof target.pattern !== "string") {
		return createErrorResult("a target needs an href or a pattern");
	}
	const metadata = getCurrentCtx()?.bootMetadata;
	const built = buildHref({
		kind: "view",
		pattern: target.pattern,
		params: target.params,
		splatValues: target.splatValues,
		search: { kind: "input", input: target.input },
		hash: target.hash,
		skewProtectionEnabled:
			metadata?.kind === "read" &&
			metadata.metadata.deploymentId !== undefined,
	});
	return built.kind === "ok" ? built : createErrorResult(built.error.msg);
}

/** Whether `navigate` handles the URL in this document: HTTP or HTTPS on the document's origin. */
export function checkNavigableOrigin(url: URL): boolean {
	return (
		(url.protocol === "http:" || url.protocol === "https:") &&
		url.origin === window.location.origin
	);
}

/** Whether the client, not the browser, navigates to the href: it parses, and it is navigable. */
export function checkNavigableHref(href: string | undefined): boolean {
	return (
		href !== undefined &&
		URL.canParse(href, window.location.href) &&
		checkNavigableOrigin(new URL(href, window.location.href))
	);
}

/** The path and search that identify a destination; hashes do not. */
export function readDestination(url: URL): string {
	return `${url.pathname}${url.search}`;
}
