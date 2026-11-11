import {
	callWithResult,
	createErrorResult,
	createSuccessResult,
	type Result,
} from "../../results/index.ts";

/** A scalar form value. Null and undefined omit a submitted part. */
export type FormFieldValue =
	| string
	| number
	| bigint
	| boolean
	| Blob
	| null
	| undefined;

/** Form fields by name; an array repeats its name once for each value. */
export type FormFields = Readonly<
	Record<string, FormFieldValue | ReadonlyArray<FormFieldValue>>
>;

/** Why an object could not be converted to native FormData. */
export type FormDataError =
	| { readonly kind: "invalid-field"; readonly fieldName: string }
	| { readonly kind: "failed"; readonly cause: unknown };

/**
 * Converts an object's own enumerable fields to native FormData. Scalars become
 * text, Blob and File values remain files, arrays repeat a name in order, and
 * null or undefined values are omitted. Empty strings remain present. A File
 * retains its filename; a Blob uses the platform's default filename.
 *
 * Values must be FormFieldValue or a readonly array of FormFieldValue. Nested
 * arrays, other objects, and numbers that are not finite return an
 * invalid-field error instead of undergoing string conversion. Accessor and
 * platform exceptions return a failed result. The function requires native
 * FormData and Blob APIs, including in workers.
 */
export function createFormData(
	fields: FormFields,
): Result<FormData, FormDataError> {
	const converted = callWithResult((): Result<FormData, FormDataError> => {
		const form = new FormData();
		for (const [name, value] of Object.entries(fields)) {
			for (const part of Array.isArray(value) ? value : [value]) {
				if (part === undefined || part === null) {
					continue;
				}
				if (typeof part === "number" && !Number.isFinite(part)) {
					return createErrorResult({
						kind: "invalid-field",
						fieldName: name,
					});
				}
				if (
					typeof part === "string" ||
					typeof part === "number" ||
					typeof part === "bigint" ||
					typeof part === "boolean"
				) {
					form.append(name, String(part));
					continue;
				}
				// The native brand check accepts Blob and File values from another
				// window without trusting a submitted toStringTag or constructor.
				const blob = callWithResult(() =>
					Blob.prototype.slice.call(part, 0, 0),
				);
				if (blob.kind === "error") {
					return createErrorResult({
						kind: "invalid-field",
						fieldName: name,
					});
				}
				form.append(name, part as Blob);
			}
		}
		return createSuccessResult(form);
	});
	return converted.kind === "error"
		? createErrorResult({ kind: "failed", cause: converted.error })
		: converted.value;
}
