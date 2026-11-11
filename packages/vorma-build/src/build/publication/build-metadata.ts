/*
The shapes of the build's own output. The build writes them to the build
directory, and the Rust server reads them.
*/

/**
 * The format of `metadata.json`. The Rust server checks it when it reads the
 * file and refuses a file of another format. It changes with any change to the
 * file's shape that an older reader or writer would misread, and the `vorma`
 * crate's `BUILD_METADATA_FORMAT_VERSION` has the same value.
 */
export const BUILD_METADATA_FORMAT_VERSION = 1;

/** One browser module's build-to-runtime projection. A view module's default export is its view. The metadata leaves out a list that is empty. */
export type ClientModule = {
	readonly importUrl: string;
	readonly depUrls?: ReadonlyArray<string>;
	readonly cssBundleUrls?: ReadonlyArray<string>;
	/** The URLs of the static public assets that the module's JavaScript references, not those that only its stylesheets reference. The server preloads the images and fonts among them. */
	readonly referencedStaticPublicAssetUrls?: ReadonlyArray<string>;
};

/** Makes a module's metadata, which leaves out each list that is empty. */
export function createClientModule(input: {
	readonly importUrl: string;
	readonly depUrls: ReadonlyArray<string>;
	readonly cssBundleUrls: ReadonlyArray<string>;
	readonly referencedStaticPublicAssetUrls: ReadonlyArray<string>;
}): ClientModule {
	return {
		importUrl: input.importUrl,
		...(input.depUrls.length === 0 ? {} : { depUrls: input.depUrls }),
		...(input.cssBundleUrls.length === 0
			? {}
			: { cssBundleUrls: input.cssBundleUrls }),
		...(input.referencedStaticPublicAssetUrls.length === 0
			? {}
			: {
					referencedStaticPublicAssetUrls:
						input.referencedStaticPublicAssetUrls,
				}),
	};
}

/** Whether the application runs as a production deployment or under the development program. */
export type RuntimeMode = "prod" | "dev";

/** Everything one build determines about its own output. The server reads it from the build directory. */
export type BuildMetadata = {
	readonly clientBuildId: string;
	readonly runtimeMode: RuntimeMode;
	/** The URL path under which the server serves the files of the public output directory, such as `/` or `/static/`. */
	readonly staticPublicAssetsServeUrlPathPrefix: string;
	/**
	 * The names of exactly the files in the public output directory, as POSIX
	 * paths in that directory. The URL of each is the prefix followed by the
	 * name as `encodeURI` spells it, which is the spelling of the URLs that
	 * the build writes; the server percent-decodes a request's path after the
	 * prefix and looks the decoded name up in this list. Every name carries
	 * Vorma's prefix, every name that Vorma chooses carries a hash of the
	 * file's content, and the server serves each file as immutable. This
	 * inventory stays on the server.
	 */
	readonly staticPublicAssetFileNames: ReadonlyArray<string>;
	/** Each static public asset's key, its exact POSIX path relative to `core.staticPublicAssets.srcDir` with no leading slash, and its published URL path. */
	readonly staticPublicAssetUrls: Readonly<Record<string, string>>;
	readonly criticalCss: string;
	/** The development HTML that Vite's plugins add to the head; absent from production metadata. */
	readonly devHeadHtml?: string;
	/** The development HTML that Vite's plugins add to the start of the body; absent from production metadata. */
	readonly devBodyPrependHtml?: string;
	/** The development HTML that Vite's plugins add to the end of the body; absent from production metadata. */
	readonly devBodyHtml?: string;
	readonly clientEntry: ClientModule;
	/** Each view's module, by the pattern that the view states. */
	readonly clientViews: Readonly<Record<string, ClientModule>>;
};
