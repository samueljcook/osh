/**
 * The lifecycle of a shared resource. A new resource is `held` by its
 * creator, with a `holdCount` of one, and each further hold adds one. The
 * release of the last hold makes it `disposed` before `disposeFn` runs.
 * `endWithoutDisposal` makes a `held` resource `ended-without-disposal`.
 * `disposed` and `ended-without-disposal` are terminal.
 */
export type SharedResourceState =
	/** At least one hold remains, so the resource is alive. */
	| { readonly kind: "held"; readonly holdCount: number }
	/**
	 * The last hold was released. The state is `disposed` from the moment
	 * `disposeFn` is called, including while it runs and after it throws.
	 */
	| { readonly kind: "disposed" }
	/** `endWithoutDisposal` ended the resource, and `disposeFn` never runs. */
	| { readonly kind: "ended-without-disposal" };

/** The outcome of a request for a further hold on a shared resource. */
export type SharedResourceHold =
	/** The hold exists, and `release` releases it. */
	| { readonly kind: "held"; readonly release: () => void }
	/** The resource is `disposed` or `ended-without-disposal`, so nothing is held. */
	| {
			readonly kind: "not-held";
			readonly reason: "disposed" | "ended-without-disposal";
	  };

/**
 * A resource that several holders share. The resource lives while a hold
 * remains, and the release of the last hold disposes it.
 *
 * Each hold has a release function. A release function releases its hold
 * once, and each later call does nothing. When it releases the last hold, the
 * resource becomes `disposed`, and then `disposeFn` runs before the function
 * returns; an error from `disposeFn` propagates to the caller of the
 * function. While a hold remains, `disposeFn` does not run. To end a hold when
 * a resource scope is disposed, register its release function with the scope.
 * Holds that a scope's disposal releases end in the scope's cleanup order.
 */
export type SharedResource = {
	readonly getState: () => SharedResourceState;
	/**
	 * Adds a hold to a `held` resource and returns it as `held`, with its
	 * release function. On a `disposed` or `ended-without-disposal` resource,
	 * including a resource while its `disposeFn` is running, this function adds no
	 * hold and returns `not-held` with the resource's state as the reason. A
	 * disposed resource never becomes held again; a holder that needs the
	 * resource after its disposal must create a new one.
	 */
	readonly hold: () => SharedResourceHold;
	/**
	 * Ends the resource without running `disposeFn`, for a resource that ended
	 * by itself or that another owner took over. Every remaining hold then
	 * holds nothing, and its release does nothing. On a `disposed` or
	 * `ended-without-disposal` resource, this function does nothing.
	 */
	readonly endWithoutDisposal: () => void;
};

/** A new shared resource and the release function of its creator's hold. */
export type CreatedSharedResource = {
	readonly sharedResource: SharedResource;
	/** Releases the hold that the creator receives with the resource. */
	readonly releaseCreatorHold: () => void;
};

/**
 * Creates a shared resource that its creator holds: the resource starts
 * `held` with one hold, and `releaseCreatorHold` releases that hold.
 * `disposeFn` must be synchronous, and it runs at most once.
 */
export function createSharedResource<DisposeReturnValue>(input: {
	readonly disposeFn: (() => DisposeReturnValue) &
		(Extract<DisposeReturnValue, PromiseLike<unknown>> extends never
			? unknown
			: never);
}): CreatedSharedResource {
	let state: SharedResourceState = { kind: "held", holdCount: 1 };

	function releaseOneHold(): void {
		if (state.kind !== "held") {
			return;
		}
		if (state.holdCount > 1) {
			state = { kind: "held", holdCount: state.holdCount - 1 };
			return;
		}
		state = { kind: "disposed" };
		input.disposeFn();
	}

	/** The release function of one hold, which releases that hold once. */
	function createRelease(): () => void {
		let ownership: "holding" | "released" = "holding";
		return () => {
			if (ownership === "released") {
				return;
			}
			ownership = "released";
			releaseOneHold();
		};
	}

	function hold(): SharedResourceHold {
		if (state.kind !== "held") {
			return { kind: "not-held", reason: state.kind };
		}
		state = { kind: "held", holdCount: state.holdCount + 1 };
		return { kind: "held", release: createRelease() };
	}

	function endWithoutDisposal(): void {
		if (state.kind === "held") {
			state = { kind: "ended-without-disposal" };
		}
	}

	return {
		sharedResource: { getState: () => state, hold, endWithoutDisposal },
		releaseCreatorHold: createRelease(),
	};
}
