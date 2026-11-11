/*
The protocol facts that the browser, the build, and generated code share.
Anything exported from `@vorma/client/unstable` is not public API: it is
unsanctioned to build on, outside semantic versioning, and can change in any
release.
*/

/** Request and response vocabulary shared by the browser and the server. */
export const HTTP_PROTOCOL = {
	queryKeys: {
		viewPayload: "vorma-json",
		deployment: "dpl",
	},
	requestHeaders: {
		clientBuildId: "x-vorma-client-build-id",
		clientAcceptsRedirect: "x-accepts-client-redirect",
		vercelDeploymentId: "x-deployment-id",
	},
	responseHeaders: {
		clientBuildId: "x-vorma-client-build-id",
		buildSkew: "x-vorma-build-skew",
		clientRedirect: "x-client-redirect",
		clientRedirectStatus: "x-client-redirect-status",
		/** Marks an endpoint success without a body, because the endpoint declares no output. */
		noOutput: "x-vorma-no-output",
	},
} as const;

/** Reserved markup names shared by the server's document and browser reconciliation. */
export const DOCUMENT_PROTOCOL = {
	elementIds: {
		root: "vorma-root",
		data: "vorma-data-json",
		criticalCss: "vorma-critical-css",
	},
	attrs: {
		cssBundle: "data-vorma-css-bundle",
		/** Marks a managed stylesheet that a preparation loaded and that does not apply until a commit applies it. */
		cssPreload: "data-vorma-css-preload",
		staticPublicAsset: "data-vorma-static-public-asset",
		linkActiveExact: "data-vorma-active-exact",
		linkActiveAncestor: "data-vorma-active-ancestor",
		linkPendingExact: "data-vorma-pending-exact",
		linkPendingAncestor: "data-vorma-pending-ancestor",
	},
	headMarkers: {
		/** The `name` of each `<meta>` element that begins a managed head region. */
		name: "vorma-head",
		/** The `content` of the marker that begins each managed head region. */
		regions: {
			title: "title",
			meta: "meta",
			rest: "rest",
			assets: "assets",
		},
	},
	/**
	 * The data of the comments that bound one raw HTML head item, each followed
	 * by the item's key: `<!--vorma-raw:KEY-->` before the item's nodes and
	 * `<!--/vorma-raw:KEY-->` after them.
	 */
	rawHtmlMarkers: {
		start: "vorma-raw:",
		end: "/vorma-raw:",
	},
} as const;

/** Persistent names owned by the browser client. */
export const BROWSER_STATE_PROTOCOL = {
	historyStateFields: {
		userState: "vorma-user-state",
		entryKey: "vorma-entry-key",
		/** The identifier of the epoch of the entry's position. */
		entryEpoch: "vorma-entry-epoch",
		/** The index of the entry's position, which increases with each entry that Vorma pushes in the same epoch. */
		entryIndex: "vorma-entry-index",
	},
	/** Session storage key holding saved scroll positions by history entry key. */
	scrollStorageKey: "vorma-scroll",
} as const;

/**
The markers of Vorma's route pattern language: a segment that starts with the
dynamic param prefix names a param, a segment that is the splat segment takes
every remaining path segment, and a trailing index segment in a view pattern
names its parent path. The server's `vorma-path-wire` crate defines the same
markers.
*/
export const PATTERN_PROTOCOL = {
	dynamicParamPrefix: ":",
	splatSegment: "*",
	indexSegment: "_index",
} as const;

/** The module that the build augments with the application's generated types. */
export const GENERATED_TYPES_PROTOCOL = {
	moduleSpecifier: "@vorma/client",
	interfaceName: "VormaGeneratedTypes",
} as const;

/** The window property through which `vorma dev` reaches the client. */
export const DEV_CONTROLS_PROTOCOL = {
	windowProperty: "__vorma_dev",
} as const;

export type RedirectStatus = 302 | 303 | 307 | 308;
