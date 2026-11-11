import { callWithResult } from "../results/index.ts";

/**
 * This operation reads a thrown string or an object's string-valued message.
 * Empty or whitespace-only messages, other values, and failed property access
 * return undefined. Nonblank text is returned unchanged.
 *
 * Reading message can invoke a getter or proxy trap, once. The operation does
 * not coerce other values to strings, serialize objects, or inspect causes or
 * stacks. Callers choose any replacement message and retain the original error
 * separately when needed.
 */
export function getErrorMsg(value: unknown): string | undefined {
	let message: unknown;
	if (typeof value === "string") {
		message = value;
	} else if (
		value !== null &&
		(typeof value === "object" || typeof value === "function")
	) {
		const property = callWithResult(() => Reflect.get(value, "message"));
		if (property.kind === "error") {
			return undefined;
		}
		message = property.value;
	}
	return typeof message === "string" && message.trim().length !== 0
		? message
		: undefined;
}
