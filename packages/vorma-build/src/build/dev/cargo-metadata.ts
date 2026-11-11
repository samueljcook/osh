import { dirname } from "node:path";
import { Effect, Schema, Stream } from "effect";
import { describeCause } from "../issues.ts";
import { startBuildProcess } from "../process.ts";

/** What Cargo reports for the server's manifest that development reads. */
export type CargoMetadata = {
	/** The directory into which Cargo builds, which every build writes. */
	readonly targetDir: string;
	/** The workspace root, which holds `Cargo.lock` and the root `Cargo.toml`. */
	readonly workspaceRoot: string;
	/**
	 * The directory of every package without a registry or Git source, such as
	 * a path dependency, among the server package and the packages that it
	 * depends on for its build, directly or through other packages.
	 */
	readonly localPackageDirs: ReadonlyArray<string>;
};

/** Cargo could not report its metadata; the message includes Cargo's own output. */
export type CargoMetadataIssue = {
	readonly _tag: "CargoMetadataIssue";
	readonly msg: string;
};

const CargoMetadataOutput = Schema.fromJsonString(
	Schema.Struct({
		target_directory: Schema.String,
		workspace_root: Schema.String,
		packages: Schema.Array(
			Schema.Struct({
				id: Schema.String,
				manifest_path: Schema.String,
				source: Schema.NullOr(Schema.String),
			}),
		),
		resolve: Schema.Struct({
			root: Schema.NullOr(Schema.String),
			nodes: Schema.Array(
				Schema.Struct({
					id: Schema.String,
					deps: Schema.Array(
						Schema.Struct({
							pkg: Schema.String,
							dep_kinds: Schema.Array(
								Schema.Struct({
									kind: Schema.NullOr(Schema.String),
								}),
							),
						}),
					),
				}),
			),
		}),
	}),
);

type CargoMetadataOutput = typeof CargoMetadataOutput.Type;

/**
 * The directories of the local packages that building the root package
 * compiles: the root and every package that it reaches through normal and build
 * dependencies.
 */
function findLocalPackageDirs(
	output: CargoMetadataOutput,
): ReadonlyArray<string> {
	const root = output.resolve.root;
	if (root === null) {
		return [];
	}
	const nodes = new Map(output.resolve.nodes.map((node) => [node.id, node]));
	const reached = new Set<string>();
	const pending = [root];
	while (pending.length > 0) {
		const id = pending.pop();
		if (id === undefined || reached.has(id)) {
			continue;
		}
		reached.add(id);
		for (const dep of nodes.get(id)?.deps ?? []) {
			if (dep.dep_kinds.some((depKind) => depKind.kind !== "dev")) {
				pending.push(dep.pkg);
			}
		}
	}
	return output.packages
		.filter((pkg) => pkg.source === null && reached.has(pkg.id))
		.map((pkg) => dirname(pkg.manifest_path));
}

/**
 * Cargo's target directory, workspace root, and local packages for a
 * manifest. Cargo resolves the target directory from `CARGO_TARGET_DIR`, its
 * configuration files, and the workspace root. Cargo reports this metadata
 * when the packages do not compile, so a development session that fails to
 * build still knows which directories hold the server's sources.
 */
export function readCargoMetadata(input: {
	readonly cargoManifestFilePath: string;
	readonly configDir: string;
}) {
	function fail(msg: string) {
		return Effect.fail({
			_tag: "CargoMetadataIssue",
			msg,
		} satisfies CargoMetadataIssue);
	}
	return Effect.scoped(
		Effect.gen(function* () {
			const childProcess = yield* startBuildProcess({
				cmd: [
					"cargo",
					"metadata",
					"--format-version=1",
					"--manifest-path",
					input.cargoManifestFilePath,
				],
				configDir: input.configDir,
				environment: {},
				stdout: "pipe",
				stderr: "pipe",
			});
			const [stdout, stderr, exitCode] = yield* Effect.all(
				[
					childProcess.stdout.pipe(
						Stream.decodeText(),
						Stream.mkString,
					),
					childProcess.stderr.pipe(
						Stream.decodeText(),
						Stream.mkString,
					),
					childProcess.exitCode,
				],
				{ concurrency: "unbounded" },
			);
			if (exitCode !== 0) {
				return yield* fail(
					`cargo metadata failed with exit code ${exitCode}${stderr.trim() === "" ? "" : `:\n${stderr.trim()}`}`,
				);
			}
			const output = yield* Schema.decodeUnknownEffect(
				CargoMetadataOutput,
			)(stdout).pipe(
				Effect.catch((cause) => {
					return fail(
						`cargo metadata did not report a target directory, a workspace root, and the resolved packages: ${describeCause(cause)}`,
					);
				}),
			);
			return {
				targetDir: output.target_directory,
				workspaceRoot: output.workspace_root,
				localPackageDirs: findLocalPackageDirs(output),
			} satisfies CargoMetadata;
		}),
	).pipe(
		Effect.catchTag("BuildProcessIssue", (cause) => {
			return fail(`Vorma could not run cargo: ${describeCause(cause)}`);
		}),
	);
}
