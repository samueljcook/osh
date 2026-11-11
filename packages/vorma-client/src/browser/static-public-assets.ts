import { type ResourceScope } from "primitives-ts/resource-scopes";
import {
	createSharedResource,
	type SharedResource,
} from "primitives-ts/shared-resources";
import { DOCUMENT_PROTOCOL } from "../protocol/constants.ts";
import type { StaticPublicAssetPreload } from "../protocol/wire.ts";

/**
 * The shared preload of each preload link that the client added, which the
 * scopes hold. The link stays in the head while a hold remains, and the
 * release of the last hold removes it. The client finds a shared preload only
 * through its link in the head, so nothing reaches a shared preload after its
 * link is removed.
 */
const sharedPreloads = new WeakMap<HTMLLinkElement, SharedResource>();
const STATIC_PUBLIC_ASSET_ATTR = DOCUMENT_PROTOCOL.attrs.staticPublicAsset;

/**
 * Adds a shared preload hint for each static public asset, in order, with the
 * request destination that the server sent for its URL, held by the scope,
 * without awaiting resource loading.
 */
export function preloadStaticPublicAssets(
	preloads: ReadonlyArray<StaticPublicAssetPreload>,
	scope: ResourceScope,
): void {
	for (const { url, destination } of preloads) {
		const existing = [
			...document.head.querySelectorAll<HTMLLinkElement>("link"),
		].find((link) => link.getAttribute(STATIC_PUBLIC_ASSET_ATTR) === url);
		if (existing !== undefined) {
			const sharedPreload = sharedPreloads.get(existing);
			// Initial-document hints belong to the document, not to a scope.
			if (sharedPreload !== undefined) {
				const hold = sharedPreload.hold();
				// The type alone requires this check: a link in the head has a held
				// shared preload, because the disposal of a shared preload removes
				// its link in the same call, and nothing ends one without disposal.
				if (hold.kind === "held") {
					scope.addCleanup(hold.release);
				}
			}
			continue;
		}
		const link = document.createElement("link");
		link.rel = "preload";
		link.as = destination;
		if (destination !== "image") {
			link.crossOrigin = "anonymous";
		}
		link.href = url;
		link.setAttribute(STATIC_PUBLIC_ASSET_ATTR, url);
		const { sharedResource: sharedPreload, releaseCreatorHold } =
			createSharedResource({
				disposeFn: () => link.remove(),
			});
		sharedPreloads.set(link, sharedPreload);
		document.head.append(link);
		scope.addCleanup(releaseCreatorHold);
	}
}
