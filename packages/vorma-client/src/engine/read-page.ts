import { awaitWithAbort } from "primitives-ts/async-work/cancellation";
import { fetchWithResult } from "primitives-ts/http/fetch";
import { type ResourceScope } from "primitives-ts/resource-scopes";
import { callWithResult } from "primitives-ts/results";
import { HTTP_PROTOCOL, type RedirectStatus } from "../protocol/constants.ts";
import {
	readNavPayload,
	readRedirectStatus,
	type NavPayload,
} from "../protocol/wire.ts";
import type { BuildSkew, RequestKind } from "../results.ts";
import { getBootMetadata } from "./boot-metadata.ts";
import type { BootMetadata, ClientCtx } from "./ctx.ts";

export type PageRead =
	| { readonly kind: "payload"; readonly payload: NavPayload }
	/** Vorma's redirect of the read. */
	| {
			readonly kind: "redirect";
			readonly href: string;
			readonly status: RedirectStatus;
	  }
	| { readonly kind: "build-skew"; readonly skew: BuildSkew }
	/**
	 * The response is not Vorma's page payload: a redirect that Vorma did not
	 * make, a response without Vorma's build ID, or a response that is not
	 * successful and carries no JSON payload.
	 */
	| { readonly kind: "document" }
	| { readonly kind: "failure"; readonly cause: unknown }
	| { readonly kind: "aborted" };

const REQUEST_HEADERS = HTTP_PROTOCOL.requestHeaders;
const RESPONSE_HEADERS = HTTP_PROTOCOL.responseHeaders;
const QUERY_KEYS = HTTP_PROTOCOL.queryKeys;

/**
 * The query text of `search` without the pairs that name a key in `keys`, and
 * without a leading question mark. Every other pair keeps its spelling, so
 * that the server sees the query of a client read as it sees the query of a
 * document request for the same URL. A pair's key is decoded as the server
 * decodes it.
 */
function removeQueryPairs(input: {
	readonly search: string;
	readonly keys: ReadonlySet<string>;
}): string {
	const query = input.search.startsWith("?")
		? input.search.slice(1)
		: input.search;
	if (query === "") {
		return "";
	}
	return query
		.split("&")
		.filter((pair) => {
			// The leading `&` keeps a `?` at the start of the pair in its key.
			const key = new URLSearchParams(`&${pair}`).keys().next();
			return key.done === true || !input.keys.has(key.value);
		})
		.join("&");
}

/**
 * The keys that Vorma's transport adds to a page address. The deployment key
 * is Vercel's, and it is Vorma's only while skew protection is on, which is
 * when the client holds a deployment ID.
 */
function listTransportKeys(metadata: BootMetadata): ReadonlySet<string> {
	return new Set(
		metadata.deploymentId === undefined
			? [QUERY_KEYS.viewPayload]
			: [QUERY_KEYS.viewPayload, QUERY_KEYS.deployment],
	);
}

/** The URL without the keys that Vorma's transport adds to a page address. */
function stripTransportKeys(input: {
	readonly url: URL;
	readonly metadata: BootMetadata;
}): URL {
	const stripped = new URL(input.url.href);
	stripped.search = removeQueryPairs({
		search: input.url.search,
		keys: listTransportKeys(input.metadata),
	});
	return stripped;
}

/**
 * The page request's URL: the destination without its hash, with the build ID
 * key appended, and, for a revalidation while skew protection is on, the
 * deployment key. A key of the same name in the destination is removed first.
 */
function buildPageRequestUrl(input: {
	readonly url: URL;
	readonly metadata: BootMetadata;
	readonly requestKind: Extract<
		RequestKind,
		"nav" | "prefetch" | "revalidation"
	>;
}): URL {
	const { metadata } = input;
	const transportPairs: ReadonlyArray<readonly [string, string]> = [
		[QUERY_KEYS.viewPayload, metadata.clientBuildId],
		...(input.requestKind === "revalidation" &&
		metadata.deploymentId !== undefined
			? [[QUERY_KEYS.deployment, metadata.deploymentId] as const]
			: []),
	];
	const kept = removeQueryPairs({
		search: input.url.search,
		keys: new Set(transportPairs.map(([key]) => key)),
	});
	const appended = transportPairs
		.map(([key, value]) => `${key}=${encodeURIComponent(value)}`)
		.join("&");
	const requestUrl = new URL(input.url.href);
	requestUrl.hash = "";
	requestUrl.search = kept === "" ? appended : `${kept}&${appended}`;
	return requestUrl;
}

/**
 * Builds the page request for a destination, runs the application's request
 * preparation, sends it, and classifies the response. The request never
 * follows an HTTP redirect itself: a redirect that Vorma did not make, such as
 * an authentication proxy's, makes the read a `document` read, so that a
 * navigation loads the requested URL as a document and the browser follows the
 * redirect.
 */
export async function readPage(
	ctx: ClientCtx,
	input: {
		readonly url: URL;
		readonly requestKind: Extract<
			RequestKind,
			"nav" | "prefetch" | "revalidation"
		>;
		readonly scope: ResourceScope;
	},
): Promise<PageRead> {
	const metadata = getBootMetadata(ctx);
	const requestUrl = buildPageRequestUrl({
		url: input.url,
		metadata,
		requestKind: input.requestKind,
	});
	const built = callWithResult(() => {
		return new Request(requestUrl, {
			headers: {
				accept: "application/json",
				[REQUEST_HEADERS.clientAcceptsRedirect]: "1",
			},
			redirect: "manual",
		});
	});
	if (built.kind === "error") {
		return { kind: "failure", cause: built.error };
	}
	let request = built.value;
	const prepare = ctx.options.prepareRequestFn;
	if (prepare !== undefined) {
		const prepared = await awaitWithAbort({
			signal: input.scope.signal,
			operationFn: () => {
				return prepare({
					request,
					requestKind: input.requestKind,
					callId: undefined,
				});
			},
		});
		if (prepared.kind === "error") {
			return prepared.error.kind === "aborted"
				? { kind: "aborted" }
				: { kind: "failure", cause: prepared.error.cause };
		}
		request = prepared.value;
	}
	const sent = await fetchWithResult({
		request,
		// A prepared request keeps the manual redirect mode.
		options: { signal: input.scope.signal, redirect: "manual" },
	});
	if (sent.kind === "error") {
		return sent.error.kind === "aborted"
			? { kind: "aborted" }
			: { kind: "failure", cause: sent.error.cause };
	}
	if (input.scope.signal.aborted) {
		return { kind: "aborted" };
	}
	const response = sent.value;
	if (response.type === "opaqueredirect") {
		return { kind: "document" };
	}
	const strippedRequestUrl = stripTransportKeys({
		url: requestUrl,
		metadata,
	});
	const requestedHref = `${strippedRequestUrl.pathname}${strippedRequestUrl.search}`;
	const serverBuildId = response.headers.get(RESPONSE_HEADERS.clientBuildId);
	if (response.headers.get(RESPONSE_HEADERS.buildSkew) !== null) {
		return {
			kind: "build-skew",
			skew: {
				clientBuildId: metadata.clientBuildId,
				serverBuildId: serverBuildId ?? undefined,
				requestedHref,
				requestKind: input.requestKind,
				status: response.status,
			},
		};
	}
	const redirectTarget = response.headers.get(
		RESPONSE_HEADERS.clientRedirect,
	);
	if (redirectTarget !== null) {
		const status = readRedirectStatus(
			response.headers.get(RESPONSE_HEADERS.clientRedirectStatus),
		);
		if (status.kind === "error") {
			return { kind: "failure", cause: status.error };
		}
		// A reference resolves against the destination that the client asked
		// for, which carries none of the transport's query keys.
		const resolved = callWithResult(() => {
			const base = stripTransportKeys({
				url: new URL(response.url || requestUrl.href),
				metadata,
			});
			return new URL(redirectTarget, base);
		});
		return resolved.kind === "ok"
			? {
					kind: "redirect",
					href: resolved.value.href,
					status: status.value,
				}
			: { kind: "failure", cause: resolved.error };
	}
	if (serverBuildId === null) {
		return { kind: "document" };
	}
	if (serverBuildId !== metadata.clientBuildId) {
		// The payload's module URLs belong to the other build's module graph.
		return {
			kind: "build-skew",
			skew: {
				clientBuildId: metadata.clientBuildId,
				serverBuildId,
				requestedHref,
				requestKind: input.requestKind,
				status: response.status,
			},
		};
	}
	const contentType = response.headers.get("content-type") ?? "";
	const isJson = contentType.toLowerCase().startsWith("application/json");
	if (!response.ok && !isJson) {
		// The document request for the same URL shows the server's answer.
		return { kind: "document" };
	}
	if (!isJson) {
		return {
			kind: "failure",
			cause: new Error(
				`The page request answered with the content type ${JSON.stringify(contentType)}.`,
			),
		};
	}
	const text = await awaitWithAbort({
		signal: input.scope.signal,
		operationFn: () => response.text(),
	});
	if (text.kind === "error") {
		return text.error.kind === "aborted"
			? { kind: "aborted" }
			: { kind: "failure", cause: text.error.cause };
	}
	if (input.scope.signal.aborted) {
		return { kind: "aborted" };
	}
	const parsed = callWithResult(() => JSON.parse(text.value) as unknown);
	if (parsed.kind === "error") {
		return { kind: "failure", cause: parsed.error };
	}
	const payload = readNavPayload(parsed.value);
	if (payload.kind === "error") {
		return { kind: "failure", cause: payload.error };
	}
	return { kind: "payload", payload: payload.value };
}
