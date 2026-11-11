import { Effect, Path } from "effect";
import { bundleClient } from "./bundling/bundle-client.ts";
import type { ResolvedVormaConfig } from "./config/resolve-config.ts";
import { loadApplicationSourceLayout } from "./config/source-layout.ts";
import {
	prepareApplicationGeneration,
	writeApplicationGeneration,
} from "./generation/generate-application.ts";
import { checkStartedRustServer } from "./inspection/rust-server.ts";
import { toBuildIssues } from "./issues.ts";
import {
	acquireOutputOwnership,
	getStagedBuildDirPaths,
	createStagedBuildDir,
} from "./output/output-ownership.ts";
import { assembleApplicationArtifacts } from "./publication/assemble-artifacts.ts";
import { writeBuildMetadata } from "./publication/publish-artifacts.ts";

/**
 * Builds and publishes one production application while holding the build
 * directory's lock, as `vorma build` does. `resolved` is the decoded and
 * resolved configuration, in which every configured path is resolved against
 * the directory of the configuration file that `core.importMetaUrl`
 * identifies. The Vite configuration is the one that `client.viteConfigFile`
 * names. Its `createVormaPlugin({ config })` plugin receives the same configuration. The
 * build writes into a staging directory inside the build directory and
 * publishes it when it completes. It fails at the first step that fails, and
 * each issue is described by its message. The check of the configured paths,
 * the inspection of the client and the server, and the server's check run
 * report every issue that they find; the other steps report their first.
 */
export function buildVormaApplication(input: {
	readonly resolved: ResolvedVormaConfig;
}) {
	return Effect.scoped(
		Effect.gen(function* () {
			const path = yield* Path.Path;
			// A production build reads no development settings, so it does not
			// check their paths.
			const sourceLayout = yield* loadApplicationSourceLayout({
				...input.resolved,
				dev: undefined,
			});
			const owned = yield* acquireOutputOwnership({
				buildDir: sourceLayout.buildDir,
				kind: "build",
			});
			const staged = yield* createStagedBuildDir({ owned });
			const stagedSourceLayout = {
				...sourceLayout,
				...getStagedBuildDirPaths({ path, staged }),
			};
			const generation =
				yield* prepareApplicationGeneration(stagedSourceLayout);
			yield* writeApplicationGeneration(generation);
			const client = yield* bundleClient(generation);
			const artifacts = yield* assembleApplicationArtifacts({
				generation,
				client,
			});
			const metadata = yield* writeBuildMetadata({
				generation,
				artifacts,
			});
			const check = yield* checkStartedRustServer({
				cmd: stagedSourceLayout.serverStartCmd,
				configDir: stagedSourceLayout.configDir,
				buildDir: stagedSourceLayout.buildDir,
			});
			yield* staged.publish({ metadata, check });
			return { clientBuildId: artifacts.metadata.clientBuildId };
		}),
	).pipe(Effect.mapError(toBuildIssues));
}
