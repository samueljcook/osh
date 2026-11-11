import {
	callWithResult,
	mapResultError,
	type Result,
} from "../../results/index.ts";

/** The Location value is not a URL reference relative to the request URL. */
export type RedirectUrlError = {
	readonly kind: "invalid-location";
	readonly cause: unknown;
};

/**
 * Resolves an HTTP redirect's Location against the original request URL,
 * including its client-side fragment. A Location without a fragment inherits
 * that fragment; an explicit fragment, including an empty `#`, replaces it.
 * The caller decides which destination origins and schemes it accepts.
 */
export function resolveRedirectUrl(input: {
	readonly location: string;
	readonly requestUrl: URL;
}): Result<URL, RedirectUrlError> {
	const resolved = callWithResult(() => {
		const target = new URL(input.location, input.requestUrl);
		const fragmentStart = input.requestUrl.href.indexOf("#");
		if (!target.href.includes("#") && fragmentStart !== -1) {
			target.hash = input.requestUrl.href.slice(fragmentStart);
		}
		return target;
	});
	return mapResultError(resolved, (cause): RedirectUrlError => {
		return { kind: "invalid-location", cause };
	});
}
