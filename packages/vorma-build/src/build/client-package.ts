import { findPackageJSON } from "node:module";
import { Effect, FileSystem, Result, Schema } from "effect";

/*
The build imports `@vorma/client/unstable`, an internal contract between the
members of the Vorma release family, from the application's installation. The
contract holds only between packages of one release, so `vorma build` and
`vorma dev` check that the installed `@vorma/client` has the version of the
`vorma` package before they load the modules that import it.
*/

/** The installed `@vorma/client` is missing or has another version than the `vorma` package. */
export type ClientPackageIssue = {
	readonly _tag: "ClientPackageIssue";
	readonly msg: string;
};

const PackageManifest = Schema.fromJsonString(
	Schema.Struct({ version: Schema.NonEmptyString }),
);

/**
 * Checks that the `@vorma/client` that the build resolves, from the
 * `resolutionBaseUrl` module, has the version `buildPackageVersion`.
 */
export function checkClientPackageVersion(input: {
	/** The version of the `vorma` package. */
	readonly buildPackageVersion: string;
	/** The URL of a module of the `vorma` package, from which the build resolves `@vorma/client`. */
	readonly resolutionBaseUrl: string;
}): Effect.Effect<void, ClientPackageIssue, FileSystem.FileSystem> {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const install = `Install @vorma/client ${input.buildPackageVersion} in the application, the version of the vorma package.`;
		const manifestPath = Result.try(() => {
			return findPackageJSON("@vorma/client", input.resolutionBaseUrl);
		});
		if (
			Result.isFailure(manifestPath) ||
			manifestPath.success === undefined
		) {
			return yield* Effect.fail({
				_tag: "ClientPackageIssue",
				msg: `vorma ${input.buildPackageVersion} needs @vorma/client, which is not installed. ${install}`,
			} satisfies ClientPackageIssue);
		}
		const manifest = yield* fileSystem
			.readFileString(manifestPath.success)
			.pipe(
				Effect.flatMap(Schema.decodeUnknownEffect(PackageManifest)),
				Effect.mapError((cause): ClientPackageIssue => {
					return {
						_tag: "ClientPackageIssue",
						msg: `vorma ${input.buildPackageVersion} cannot read the version of the installed @vorma/client from ${manifestPath.success}: ${cause.message}. ${install}`,
					};
				}),
			);
		if (manifest.version !== input.buildPackageVersion) {
			return yield* Effect.fail({
				_tag: "ClientPackageIssue",
				msg: `vorma ${input.buildPackageVersion} needs @vorma/client ${input.buildPackageVersion}, but the installed @vorma/client is ${manifest.version}. ${install}`,
			} satisfies ClientPackageIssue);
		}
	});
}
