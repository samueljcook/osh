import { callWithResult } from "primitives-ts/results";
import type { ServerFailure } from "../protocol/wire.ts";
import type { PageError } from "../results.ts";

const GENERIC_CLIENT_MSG = "An unexpected error occurred.";

/** The status and client message of a thrown value: an object with a numeric `status` and a string `clientMsg`, else 500 and a generic message. */
function readBrowserFailureFields(cause: unknown): {
	readonly status: number;
	readonly clientMsg: string;
} {
	const fields = callWithResult(() => {
		if (
			typeof cause !== "object" ||
			cause === null ||
			!("status" in cause) ||
			!("clientMsg" in cause)
		) {
			return undefined;
		}
		const { status, clientMsg } = cause;
		return typeof status === "number" && typeof clientMsg === "string"
			? { status, clientMsg }
			: undefined;
	});
	if (fields.kind === "ok" && fields.value !== undefined) {
		return fields.value;
	}
	return { status: 500, clientMsg: GENERIC_CLIENT_MSG };
}

export function toServerPageError(failure: ServerFailure): PageError {
	switch (failure.kind) {
		case "declared-error":
			return {
				kind: "declared-error",
				declaredError: failure.declaredError,
				status: failure.status,
			};
		case "rejection":
			return {
				kind: "rejection",
				clientMsg: failure.clientMsg,
				status: failure.status,
			};
		case "undeclared-failure":
			return { kind: "undeclared-failure", status: failure.status };
	}
}

export function toBrowserPageError(
	input:
		| {
				readonly kind: "client-loader-failure";
				readonly pattern: string;
				readonly cause: unknown;
		  }
		| {
				readonly kind: "render-failure";
				readonly pattern: string | undefined;
				readonly cause: unknown;
		  },
): PageError {
	return { ...input, ...readBrowserFailureFields(input.cause) };
}

export function toBootPageError(cause: unknown): PageError {
	return { kind: "boot-failure", cause, ...readBrowserFailureFields(cause) };
}
