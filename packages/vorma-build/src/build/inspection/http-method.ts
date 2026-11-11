import * as NativeHttpMethod from "effect/http/HttpMethod";

/** Methods supported by Vorma's browser-facing endpoint protocol. */
export type HttpMethod = Exclude<NativeHttpMethod.HttpMethod, "HEAD" | "TRACE">;

export function testHttpMethod(method: unknown): method is HttpMethod {
	return (
		NativeHttpMethod.isHttpMethod(method) &&
		method !== "HEAD" &&
		method !== "TRACE"
	);
}
