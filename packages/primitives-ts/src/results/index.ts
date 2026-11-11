/** A successful result contains its returned value. */
export type SuccessResult<Value> = {
	readonly kind: "ok";
	readonly value: Value;
};

/** An error result contains its failure value without converting it. */
export type ErrorResult<Failure> = {
	readonly kind: "error";
	readonly error: Failure;
};

/** A result represents either a returned value or a typed failure. */
export type Result<Value, Failure> =
	| SuccessResult<Value>
	| ErrorResult<Failure>;

/** This constructor retains the supplied value without cloning or awaiting it. */
export function createSuccessResult<Value>(value: Value): SuccessResult<Value> {
	return { kind: "ok", value };
}

/** This constructor retains the supplied failure without wrapping or stringifying it. */
export function createErrorResult<Failure>(
	error: Failure,
): ErrorResult<Failure> {
	return { kind: "error", error };
}

/** This operation maps a successful value and returns an error result unchanged.
 * Exceptions from the callback propagate to its caller.
 */
export function mapResultValue<Value, MappedValue, Failure = never>(
	result: Result<Value, Failure>,
	mapValueFn: (value: Value) => MappedValue,
): Result<MappedValue, Failure> {
	return result.kind === "ok"
		? createSuccessResult(mapValueFn(result.value))
		: result;
}

/** This operation maps a failure and returns a successful result unchanged.
 * Exceptions from the callback propagate to its caller.
 */
export function mapResultError<Failure, MappedFailure, Value = never>(
	result: Result<Value, Failure>,
	mapErrorFn: (error: Failure) => MappedFailure,
): Result<Value, MappedFailure> {
	return result.kind === "error"
		? createErrorResult(mapErrorFn(result.error))
		: result;
}

/** This operation calls the next function on success and returns its result.
 * An earlier error result is returned unchanged. Callback exceptions propagate.
 */
export function flatMapResult<
	Value,
	Failure = never,
	NextValue = never,
	NextFailure = never,
>(
	result: Result<Value, Failure>,
	nextFn: (value: Value) => Result<NextValue, NextFailure>,
): Result<NextValue, Failure | NextFailure> {
	return result.kind === "ok" ? nextFn(result.value) : result;
}

/** This adapter captures synchronous throws without converting the thrown value.
 * It calls the function once and does not await or inspect a returned promise.
 */
export function callWithResult<Value>(
	operationFn: () => Value,
): Result<Value, unknown> {
	try {
		return createSuccessResult(operationFn());
	} catch (error) {
		return createErrorResult(error);
	}
}

/** This adapter calls the function once and awaits its returned value.
 * Synchronous throws, promise rejections, and thenable failures retain their
 * original failure values. It does not cancel the underlying operation.
 */
export async function callWithResultAsync<Value>(
	operationFn: () => Value,
): Promise<Result<Awaited<Value>, unknown>> {
	try {
		return createSuccessResult(await operationFn());
	} catch (error) {
		return createErrorResult(error);
	}
}
