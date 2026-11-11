import type * as Path from "effect/Path";

/*
The layout of the dist directory, which holds one build directory for
`vorma build` and one for `vorma dev`, and the layout of a build directory.
The build derives its artifact paths with one calculation; the Rust server
reads the same layout.
*/

const PUBLIC_OUTPUT_FILE_NAME_PREFIX = "vorma_out_";

/** The layout of the dist directory and of a build directory, and the stable names of published files. */
export const ARTIFACT_PROTOCOL = {
	/** Inside the dist directory: the build directory that `vorma build` writes and that the server reads in production. */
	prodBuildDirName: "prod",
	/** Inside the dist directory: the build directory that `vorma dev` writes and that the development server reads. */
	devBuildDirName: "dev",
	/** Inside the build directory: the build metadata that the server reads at startup. */
	buildMetadataFileName: "metadata.json",
	/** Inside the build directory: the published static public assets. */
	publicDirName: "public",
	/** Inside the build directory: the lock that a running build or development session holds. */
	lockDirName: "build.lock",
	/** Inside the build directory: the directory in which a running build stages its output. */
	stagingDirName: "staging",
	publicOutputFileNamePrefix: PUBLIC_OUTPUT_FILE_NAME_PREFIX,
	/**
	 * The Rolldown output options that name the client build's files. Each
	 * name carries Vorma's prefix and a hash that covers the file's content, so
	 * a published file with a given name always has the same bytes. A source
	 * map carries the hash of its chunk, which covers the original text of the
	 * chunk's modules when the build writes source maps.
	 */
	clientOutputFileNames: {
		entryFileNames: `${PUBLIC_OUTPUT_FILE_NAME_PREFIX}[name]-[hash].js`,
		chunkFileNames: `${PUBLIC_OUTPUT_FILE_NAME_PREFIX}[name]-[hash].js`,
		assetFileNames: `${PUBLIC_OUTPUT_FILE_NAME_PREFIX}[name]-[hash][extname]`,
		sourcemapFileNames: `${PUBLIC_OUTPUT_FILE_NAME_PREFIX}[name]-[chunkhash].js.map`,
	},
} as const;

export type BuildDirPaths = {
	/** The one directory that the build writes. */
	readonly buildDir: string;
	/** The published static public assets. */
	readonly publicDir: string;
	/** The build metadata that the server reads. */
	readonly buildMetadataFilePath: string;
};

/** The path operations that the derivation needs; both Effect Path and `node:path` provide them. */
type BuildDirPathOperations = Pick<Path.Path, "join">;

export function getBuildDirPaths(input: {
	readonly path: BuildDirPathOperations;
	/** The absolute path of the build directory. */
	readonly buildDir: string;
}): BuildDirPaths {
	const { path, buildDir } = input;
	return {
		buildDir,
		publicDir: path.join(buildDir, ARTIFACT_PROTOCOL.publicDirName),
		buildMetadataFilePath: path.join(
			buildDir,
			ARTIFACT_PROTOCOL.buildMetadataFileName,
		),
	};
}
