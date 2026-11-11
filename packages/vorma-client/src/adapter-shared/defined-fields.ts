/**
 * The fields of an object that are not `undefined`. Options with
 * `exactOptionalPropertyTypes` accept an absent field and reject an explicit
 * `undefined`, while component props carry explicit `undefined` values.
 */
export function pickDefinedFields<Value extends object>(
	value: Value,
): { [Key in keyof Value]?: Exclude<Value[Key], undefined> } {
	return Object.fromEntries(
		Object.entries(value).filter(([, field]) => field !== undefined),
	) as { [Key in keyof Value]?: Exclude<Value[Key], undefined> };
}
