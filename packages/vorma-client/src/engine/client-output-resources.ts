import type { ResourceScope } from "primitives-ts/resource-scopes";
import {
	createSharedResource,
	type SharedResourceHold,
} from "primitives-ts/shared-resources";
import type { ClientOutputScope } from "../views.ts";
import type { RouteEntry } from "./core-state.ts";

/** The preparation, frame, and cache references to one client-loader output. */
export type ClientOutputResources = {
	readonly outputScope: ClientOutputScope;
	readonly used: boolean;
	/** Adds a hold on the output; a closed output gives `not-held`. */
	readonly retain: () => SharedResourceHold;
};

export function createClientOutputResources(input: {
	readonly preparationScope: ResourceScope;
	readonly documentScope: ResourceScope;
}): ClientOutputResources {
	type OutputState =
		| { readonly kind: "dormant" }
		| {
				readonly kind: "active";
				readonly scope: ResourceScope;
				readonly publicScope: ClientOutputScope;
		  }
		| { readonly kind: "closed-unused" }
		| {
				readonly kind: "closed-used";
				readonly publicScope: ClientOutputScope;
		  };
	let state: OutputState = { kind: "dormant" };
	// Each reference to the output is a hold, and the release of the last hold
	// closes the output. The preparation's hold is the creator's hold.
	const { sharedResource: sharedOutput, releaseCreatorHold } =
		createSharedResource({
			disposeFn: () => {
				const previous = state;
				if (previous.kind === "dormant") {
					state = { kind: "closed-unused" };
				} else if (previous.kind === "active") {
					state = {
						kind: "closed-used",
						publicScope: previous.publicScope,
					};
					previous.scope.dispose();
				}
			},
		});

	input.preparationScope.addCleanup(releaseCreatorHold);
	return {
		get outputScope() {
			const previous = state;
			if (previous.kind === "active" || previous.kind === "closed-used") {
				return previous.publicScope;
			}
			const scope = input.documentScope.createChild();
			const publicScope: ClientOutputScope = {
				signal: scope.signal,
				addCleanup: scope.addCleanup,
			};
			if (previous.kind === "dormant") {
				state = { kind: "active", scope, publicScope };
			} else {
				state = { kind: "closed-used", publicScope };
				scope.dispose();
			}
			return publicScope;
		},
		get used() {
			return state.kind === "active" || state.kind === "closed-used";
		},
		retain: sharedOutput.hold,
	};
}

/** Keeps each output alive until this frame or cached page releases it. */
export function retainClientOutputs(
	entries: ReadonlyArray<RouteEntry>,
): () => void {
	const resources = new Set(
		entries.map((entry) => entry.clientOutputResources),
	);
	const releases = [...resources].flatMap((resource) => {
		if (resource === undefined) {
			return [];
		}
		const hold = resource.retain();
		// The type alone requires this check: every caller retains outputs that
		// something still holds, namely the preparation or candidate that
		// commits them, the current frame, or the cache.
		return hold.kind === "held" ? [hold.release] : [];
	});
	return () => {
		for (const release of releases) {
			release();
		}
	};
}
