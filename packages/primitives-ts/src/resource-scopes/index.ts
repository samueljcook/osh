import { callWithResult } from "../results/index.ts";

type ScopeRegistration = {
	state:
		| { readonly kind: "cleanup"; readonly cleanupFn: () => unknown }
		| {
				readonly kind: "child";
				readonly disposeChildInto: (options: {
					readonly reason: unknown;
					readonly reportingErrors: Array<unknown>;
				}) => void;
		  }
		| { readonly kind: "removed" }
		| { readonly kind: "executed" };
};

type ScopeState =
	| { readonly kind: "open"; readonly cleanups: Array<ScopeRegistration> }
	| {
			readonly kind: "disposing";
			readonly cleanups: Array<ScopeRegistration>;
	  }
	| { readonly kind: "disposed" };

type InternalScopeNode = {
	readonly scope: ResourceScope;
	readonly disposeInto: (options: {
		readonly reason: unknown;
		readonly reportingErrors: Array<unknown>;
	}) => void;
};

/** A resource scope owns cancellation and synchronous cleanup for a group of work. */
export type ResourceScope = {
	readonly signal: AbortSignal;
	/**
	 * Registers one synchronous cleanup and returns a function that removes that
	 * registration without running it. Repeated callbacks register independently.
	 * After disposal, the cleanup runs immediately, and this function throws the
	 * AggregateError that `dispose` would throw for it.
	 */
	readonly addCleanup: <ReturnValue>(
		cleanupFn: (() => ReturnValue) &
			(Extract<ReturnValue, PromiseLike<unknown>> extends never
				? unknown
				: never),
	) => () => void;
	/**
	 * Creates a child that inherits this scope's cleanup-error handler. Parent
	 * disposal disposes the child with the same reason. Child disposal detaches
	 * it without disposing its parent or siblings. A child created after parent
	 * disposal is already disposed.
	 */
	readonly createChild: () => ResourceScope;
	/**
	 * Aborts the signal and runs registered cleanups once in reverse order.
	 * Repeated disposal does nothing. When the cleanup-error handler throws,
	 * disposal runs every remaining cleanup and then throws an AggregateError.
	 */
	readonly dispose: (reason?: unknown) => void;
};

function throwReportingErrors(errors: Array<unknown>): void {
	if (errors.length > 0) {
		throw new AggregateError(
			errors,
			"One or more resource cleanups failed.",
		);
	}
}

function createScopeNode(input: {
	readonly onCleanupErrorFn: (error: unknown) => unknown;
	readonly onDisposeStartFn?: () => void;
}): InternalScopeNode {
	const controller = new AbortController();
	let state: ScopeState = { kind: "open", cleanups: [] };

	function runCleanup(options: {
		readonly cleanupFn: () => unknown;
		readonly reportingErrors: Array<unknown>;
	}): void {
		const result = callWithResult(options.cleanupFn);
		if (result.kind === "error") {
			const reported = callWithResult(() =>
				input.onCleanupErrorFn(result.error),
			);
			if (reported.kind === "error") {
				options.reportingErrors.push(reported.error);
			}
		}
	}

	function removeEntry(entry: ScopeRegistration): void {
		if (entry.state.kind !== "cleanup" && entry.state.kind !== "child") {
			return;
		}
		entry.state = { kind: "removed" };
		if (state.kind !== "disposed") {
			const index = state.cleanups.indexOf(entry);
			if (index !== -1) {
				state.cleanups.splice(index, 1);
			}
		}
	}

	function disposeInto(options: {
		readonly reason: unknown;
		readonly reportingErrors: Array<unknown>;
	}): void {
		if (state.kind !== "open") {
			return;
		}
		const disposing: Extract<ScopeState, { kind: "disposing" }> = {
			kind: "disposing",
			cleanups: state.cleanups,
		};
		state = disposing;
		input.onDisposeStartFn?.();
		controller.abort(options.reason);
		while (disposing.cleanups.length > 0) {
			const entry = disposing.cleanups.pop();
			if (entry?.state.kind === "cleanup") {
				const { cleanupFn } = entry.state;
				entry.state = { kind: "executed" };
				runCleanup({
					cleanupFn,
					reportingErrors: options.reportingErrors,
				});
			} else if (entry?.state.kind === "child") {
				const { disposeChildInto } = entry.state;
				entry.state = { kind: "executed" };
				disposeChildInto({
					reason: controller.signal.reason,
					reportingErrors: options.reportingErrors,
				});
			}
		}
		state = { kind: "disposed" };
	}

	function dispose(reason?: unknown): void {
		const reportingErrors: Array<unknown> = [];
		disposeInto({ reason, reportingErrors });
		throwReportingErrors(reportingErrors);
	}

	function addCleanup(cleanupFn: () => unknown): () => void {
		if (state.kind !== "open") {
			const reportingErrors: Array<unknown> = [];
			runCleanup({ cleanupFn, reportingErrors });
			throwReportingErrors(reportingErrors);
			return () => undefined;
		}
		const entry: ScopeRegistration = {
			state: { kind: "cleanup", cleanupFn },
		};
		state.cleanups.push(entry);
		return () => {
			removeEntry(entry);
		};
	}

	function createChild(): ResourceScope {
		if (state.kind !== "open") {
			const child = createScopeNode({
				onCleanupErrorFn: input.onCleanupErrorFn,
			});
			const reportingErrors: Array<unknown> = [];
			child.disposeInto({
				reason: controller.signal.reason,
				reportingErrors,
			});
			throwReportingErrors(reportingErrors);
			return child.scope;
		}
		let childEntry: ScopeRegistration | undefined;
		const child = createScopeNode({
			onCleanupErrorFn: input.onCleanupErrorFn,
			onDisposeStartFn: () => {
				if (childEntry !== undefined) {
					removeEntry(childEntry);
				}
			},
		});
		childEntry = {
			state: {
				kind: "child",
				disposeChildInto: child.disposeInto,
			},
		};
		state.cleanups.push(childEntry);
		return child.scope;
	}

	return {
		scope: { signal: controller.signal, addCleanup, createChild, dispose },
		disposeInto,
	};
}

/**
 * Creates a scope without starting work. Cleanup registered after disposal runs
 * immediately. Cleanup failures go to the supplied handler, and remaining
 * cleanups still run. The handler must be synchronous. If it throws, disposal
 * finishes cleanup and then throws an AggregateError with the values that the
 * handler threw as its `errors`, so a handler that rethrows its argument makes
 * disposal throw the cleanup failures.
 */
export function createResourceScope<ReportReturnValue>(input: {
	readonly onCleanupErrorFn: ((error: unknown) => ReportReturnValue) &
		(Extract<ReportReturnValue, PromiseLike<unknown>> extends never
			? unknown
			: never);
}): ResourceScope {
	return createScopeNode({ onCleanupErrorFn: input.onCleanupErrorFn }).scope;
}
