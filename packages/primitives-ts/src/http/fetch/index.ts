import {
	awaitWithAbort,
	type AbortableOperationError,
} from "../../async-work/cancellation/index.ts";
import {
	callWithResult,
	createErrorResult,
	type Result,
} from "../../results/index.ts";

/**
 * Constructs and sends a native Request with ordinary Fetch options. Its signal
 * controls both the network request and prompt result settlement. HTTP error
 * statuses return successful Response values. Body encoding is explicit, and
 * the request's signal continues to control response-body consumption.
 */
export function fetchWithResult(input: {
	readonly request: RequestInfo | URL;
	readonly options?: RequestInit;
}): Promise<Result<Response, AbortableOperationError>> {
	const request = callWithResult(
		() => new Request(input.request, input.options),
	);
	if (request.kind === "error") {
		return Promise.resolve(
			createErrorResult({ kind: "failed", cause: request.error }),
		);
	}
	return awaitWithAbort({
		signal: request.value.signal,
		operationFn: () => fetch(request.value),
	});
}
