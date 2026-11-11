import type { Result } from "../../../results/index.ts";
import { createErrorResult } from "../../../results/index.ts";
import { getCookieValue } from "../../cookies/browser/index.ts";
import type { CookieError } from "../../cookies/headers/index.ts";
import { COOKIE_NAME_PATTERN } from "../../cookies/syntax.ts";
/** Reads the server-issued encrypted CSRF value without decrypting or percent decoding it.
 * Attach the returned value to the configured CSRF request header. The cookie
 * name must not start with `__Host-`, `__Dev-`, or `__Secure-` in any ASCII case.
 */
export function getCsrfToken(options: {
	readonly security: "https" | "loopback-http";
	readonly cookieName: string;
}): Result<string | undefined, CookieError> {
	if (
		!COOKIE_NAME_PATTERN.test(options.cookieName) ||
		/^(?:__Host-|__Dev-|__Secure-)/i.test(options.cookieName)
	) {
		return createErrorResult({ kind: "invalid-name" });
	}
	return getCookieValue({
		name: `${options.security === "https" ? "__Host-" : "__Dev-"}${options.cookieName}`,
		encoding: "raw",
	});
}
