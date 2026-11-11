import type { ResolvedDev } from "../config/resolve-config.ts";
import type { ApplicationSourceLayout } from "../config/source-layout.ts";
import type { CargoMetadata } from "./cargo-metadata.ts";

/**
 * The source layout, what Cargo reports for the server's manifest, and the
 * paths that development writes, all of which only development reads.
 * `buildDir`, `publicDir`, and `buildMetadataFilePath` name the build
 * directory of `vorma dev`, not the build directory of `vorma build`.
 */
export type DevLayout = ApplicationSourceLayout & {
	readonly dev: ResolvedDev;
	readonly cargo: CargoMetadata;
	/** See `getGeneratedPaths`. */
	readonly generatedPaths: ReadonlyArray<string>;
};

/**
 * The paths that `vorma build`, `vorma dev`, and Cargo write: the configured
 * dist directory, the client's generated types file, and Cargo's target
 * directory. Their changes never start
 * development work. `cargoTargetDir` is `undefined` only when Cargo could not
 * report it, and the session then fails before it runs Cargo.
 */
export function getGeneratedPaths(input: {
	readonly distDir: string;
	readonly clientTypesFilePath: string;
	readonly cargoTargetDir: string | undefined;
}): ReadonlyArray<string> {
	return [
		input.distDir,
		input.clientTypesFilePath,
		...(input.cargoTargetDir === undefined ? [] : [input.cargoTargetDir]),
	];
}
