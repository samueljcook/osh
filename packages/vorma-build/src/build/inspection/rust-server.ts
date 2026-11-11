import { Effect, FileSystem, Order, Result, Schema, Stream } from "effect";
import * as Path from "effect/Path";
import { describeCause } from "../issues.ts";
import type { CheckedBuildDir } from "../output/output-ownership.ts";
import { SERVER_MODE_ENV_VARS, startBuildProcess } from "../process.ts";
import { testHttpMethod } from "./http-method.ts";
import type {
	InspectedServerDeclaration,
	InspectionIssues,
	ServerInspection,
} from "./inspect-projects.ts";
import type { TypeDeclarations } from "./type-declarations.ts";

/*
A Rust server describes its own declarations. Vorma runs the server's binary
with the contract file environment variable set; the server's `App::load` then
writes its declaration contract to that file and exits before it reads any
build output. `vorma build` runs the application's `build.serverStartCmd`.
`vorma dev` compiles the target that `dev.server` names with Cargo and runs the
compiled executable. The contract carries the TypeScript text of each
handler's types and the named declarations that the texts reference, so the
build generates client contracts without reading Rust source. The contract
also carries the hash of the declarations, which the build records in the build
directory; the server refuses a build directory with another hash.

After `vorma build` writes the build directory, it runs `build.serverStartCmd`
once more with the check environment variable set to that directory. The server's `load`
then runs every check that it runs at startup against that directory and exits,
so a problem fails the build instead of a deployment. Each run receives only
the mode variable that the build sets for it; neither variable passes from
Vorma's own environment to a command.
*/

/** The contract format that this build reads. */
const CONTRACT_FORMAT_VERSION = 1;

/** An executable and its arguments, run without a shell. */
type Cmd = readonly [string, ...Array<string>];

const TypeText = Schema.Struct({
	text: Schema.String,
	references: Schema.Array(Schema.String),
});

const HandlerTypes = Schema.Struct({
	input: Schema.NullOr(TypeText),
	output: Schema.NullOr(TypeText),
	error: Schema.NullOr(TypeText),
});

const DeclarationContract = Schema.fromJsonString(
	Schema.Struct({
		formatVersion: Schema.Number,
		declarationsHash: Schema.String,
		typeDeclarations: Schema.Array(
			Schema.Struct({
				name: Schema.String,
				declaration: Schema.String,
				references: Schema.Array(Schema.String),
			}),
		),
		viewLoaders: Schema.Array(
			Schema.Struct({
				location: Schema.String,
				pattern: Schema.String,
				types: HandlerTypes,
			}),
		),
		apiEndpoints: Schema.Array(
			Schema.Struct({
				location: Schema.String,
				apiEndpointKind: Schema.Literals(["query", "mutation"]),
				method: Schema.String.pipe(
					Schema.refine(testHttpMethod, {
						expected: "an endpoint method",
					}),
				),
				pattern: Schema.String,
				apiEndpointInputKind: Schema.Literals([
					"none",
					"search",
					"json",
					"form-data",
				]),
				types: HandlerTypes,
			}),
		),
	}),
);

type DeclarationContract = typeof DeclarationContract.Type;

/** The contract's version, which the build checks before it decodes the rest. */
const ContractFormatVersion = Schema.fromJsonString(
	Schema.Struct({ formatVersion: Schema.Number }),
);

/** A Rust server's declarations and the executable that described them. */
export type CompiledRustServer = {
	readonly inspection: ServerInspection;
	/** The compiled binary; development reads its Cargo dependency file beside it and runs it. */
	readonly executable: string;
};

function failInspection(msg: string): Effect.Effect<never, InspectionIssues> {
	return Effect.fail({
		_tag: "InspectionIssues",
		issues: [{ msg }],
	} satisfies InspectionIssues);
}

/**
 * Runs `cmd` so that the server it starts writes its declaration
 * contract, and reads the declarations. `vorma build` runs
 * `build.serverStartCmd` this way.
 */
export function inspectStartedRustServer(input: {
	readonly cmd: Cmd;
	readonly configDir: string;
}) {
	return readDeclarations({
		cmd: input.cmd,
		serverLabel: input.cmd.join(" "),
		configDir: input.configDir,
	}).pipe(
		Effect.map((inspection) => {
			return { inspection };
		}),
		Effect.scoped,
	);
}

/**
 * Compiles the server binary with Cargo and runs it with `binArgs` to read the
 * declaration contract. `vorma dev` inspects the server this way and then runs
 * the same executable.
 */
export function inspectCompiledRustServer(input: {
	readonly cargoManifestFilePath: string;
	readonly cargoBin: string;
	readonly cargoArgs: ReadonlyArray<string>;
	readonly binArgs: ReadonlyArray<string>;
	readonly configDir: string;
}) {
	return Effect.gen(function* () {
		const executable = yield* buildServerBinary(input);
		const inspection = yield* readDeclarations({
			cmd: [executable, ...input.binArgs],
			serverLabel: input.cargoBin,
			configDir: input.configDir,
		});
		return { inspection, executable } satisfies CompiledRustServer;
	}).pipe(Effect.scoped);
}

/**
 * Runs `cmd` so that the server it starts loads the build directory
 * `buildDir`, runs its startup checks, and exits. `vorma build` runs
 * `build.serverStartCmd` this way after it writes the build directory.
 */
export function checkStartedRustServer(input: {
	readonly cmd: Cmd;
	readonly configDir: string;
	readonly buildDir: string;
}) {
	return runServerMode({
		cmd: input.cmd,
		configDir: input.configDir,
		serverLabel: input.cmd.join(" "),
		environment: { [SERVER_MODE_ENV_VARS.checkBuildDir]: input.buildDir },
		purpose: "load the build directory that this build wrote",
	}).pipe(
		Effect.as({
			_tag: "CheckedBuildDir",
			buildDir: input.buildDir,
		} satisfies CheckedBuildDir),
	);
}

function readDeclarations(input: {
	readonly cmd: Cmd;
	readonly serverLabel: string;
	readonly configDir: string;
}) {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const path = yield* Path.Path;
		const contractDir = yield* fileSystem
			.makeTempDirectoryScoped({ prefix: "vorma-contract-" })
			.pipe(
				Effect.mapError((cause) => {
					return {
						_tag: "InspectionIssues",
						issues: [
							{
								msg: `Vorma could not create a directory for the server's declaration contract: ${describeCause(cause)}`,
							},
						],
					} satisfies InspectionIssues;
				}),
			);
		const contractFile = path.join(contractDir, "contract.json");
		yield* runServerMode({
			cmd: input.cmd,
			configDir: input.configDir,
			serverLabel: input.serverLabel,
			environment: { [SERVER_MODE_ENV_VARS.contractFile]: contractFile },
			purpose: "describe its declarations",
		});
		const text = yield* fileSystem.readFileString(contractFile).pipe(
			Effect.catch((cause) => {
				return failUnreadableContract({
					serverLabel: input.serverLabel,
					cause,
				});
			}),
		);
		return yield* decodeDeclarationContract({
			text,
			serverLabel: input.serverLabel,
		});
	});
}

function failUnreadableContract(input: {
	readonly serverLabel: string;
	readonly cause: unknown;
}): Effect.Effect<never, InspectionIssues> {
	return failInspection(
		`The Rust server ${input.serverLabel} did not write a declaration contract that this build reads: ${describeCause(input.cause)}`,
	);
}

/**
 * Decodes the declaration contract that the Rust server `serverLabel` wrote. The
 * contract's version is checked before the rest, so a contract of another
 * version fails with the version message.
 */
export function decodeDeclarationContract(input: {
	readonly text: string;
	readonly serverLabel: string;
}): Effect.Effect<ServerInspection, InspectionIssues> {
	return Effect.gen(function* () {
		function failUnreadable(cause: unknown) {
			return failUnreadableContract({
				serverLabel: input.serverLabel,
				cause,
			});
		}
		const { formatVersion } = yield* Schema.decodeUnknownEffect(
			ContractFormatVersion,
		)(input.text).pipe(Effect.catch(failUnreadable));
		if (formatVersion !== CONTRACT_FORMAT_VERSION) {
			return yield* failInspection(
				`The Rust server ${input.serverLabel} wrote declaration contract version ${formatVersion}, but this build reads version ${CONTRACT_FORMAT_VERSION}; use matching versions of the vorma crate and the vorma package`,
			);
		}
		const contract = yield* Schema.decodeUnknownEffect(DeclarationContract)(
			input.text,
		).pipe(Effect.catch(failUnreadable));
		return toServerInspection(contract);
	});
}

const CompilerArtifact = Schema.fromJsonString(
	Schema.Struct({
		reason: Schema.Literal("compiler-artifact"),
		target: Schema.Struct({
			name: Schema.String,
			kind: Schema.Array(Schema.String),
		}),
		executable: Schema.NullOr(Schema.String),
	}),
);

/** Builds the binary target and returns its executable. Cargo prints compiler diagnostics itself. */
function buildServerBinary(input: {
	readonly cargoManifestFilePath: string;
	readonly cargoBin: string;
	readonly cargoArgs: ReadonlyArray<string>;
	readonly configDir: string;
}) {
	return Effect.gen(function* () {
		const { cargoBin, cargoManifestFilePath } = input;
		const childProcess = yield* startBuildProcess({
			cmd: [
				"cargo",
				"build",
				"--manifest-path",
				cargoManifestFilePath,
				"--bin",
				cargoBin,
				"--message-format=json-render-diagnostics",
				...input.cargoArgs,
			],
			configDir: input.configDir,
			environment: {},
			stdout: "pipe",
			stderr: "inherit",
		});
		const [msgs, exitCode] = yield* Effect.all(
			[
				childProcess.stdout.pipe(
					Stream.decodeText(),
					Stream.splitLines,
					Stream.runCollect,
				),
				childProcess.exitCode,
			],
			{ concurrency: "unbounded" },
		);
		if (exitCode !== 0) {
			return yield* failInspection(
				`cargo build --bin ${cargoBin} failed with exit code ${exitCode}`,
			);
		}
		const executables = msgs.flatMap((line) => {
			const artifact = Schema.decodeUnknownResult(CompilerArtifact)(line);
			return Result.isSuccess(artifact) &&
				artifact.success.target.name === cargoBin &&
				artifact.success.target.kind.includes("bin") &&
				artifact.success.executable !== null
				? [artifact.success.executable]
				: [];
		});
		const executable = executables.at(-1);
		if (executable === undefined) {
			return yield* failInspection(
				`cargo build --bin ${cargoBin} reported no executable for the binary target ${cargoBin}`,
			);
		}
		return executable;
	}).pipe(
		Effect.catchTag("BuildProcessIssue", (cause) => {
			return failInspection(
				`Vorma could not run cargo to build the Rust server: ${describeCause(cause)}`,
			);
		}),
	);
}

/** Runs the server command in a mode that `environment` selects; its stderr names the problems when it fails. */
function runServerMode(input: {
	readonly cmd: Cmd;
	readonly environment: Readonly<Record<string, string>>;
	readonly configDir: string;
	readonly serverLabel: string;
	/** What the server does in this mode, as the verb phrase of a failure message. */
	readonly purpose: string;
}) {
	return Effect.gen(function* () {
		const childProcess = yield* startBuildProcess({
			cmd: input.cmd,
			configDir: input.configDir,
			environment: input.environment,
			stdout: "inherit",
			stderr: "pipe",
		});
		const [stderr, exitCode] = yield* Effect.all(
			[
				childProcess.stderr.pipe(Stream.decodeText(), Stream.mkString),
				childProcess.exitCode,
			],
			{ concurrency: "unbounded" },
		);
		if (exitCode !== 0) {
			return yield* failInspection(
				`The Rust server ${input.serverLabel} could not ${input.purpose} (exit code ${exitCode})${stderr.trim() === "" ? "" : `:\n${stderr.trim()}`}`,
			);
		}
	}).pipe(
		Effect.catchTag("BuildProcessIssue", (cause) => {
			return failInspection(
				`Vorma could not run the Rust server ${input.serverLabel}: ${describeCause(cause)}`,
			);
		}),
	);
}

/** The declarations, sorted by kind, pattern, and method, and the type names that every type text references. */
function toServerInspection(contract: DeclarationContract): ServerInspection {
	const typeTexts = new Map<string, ReadonlySet<string>>();
	function registerTypeText(
		typeText: typeof TypeText.Type | null,
	): string | undefined {
		if (typeText === null) {
			return undefined;
		}
		typeTexts.set(typeText.text, new Set(typeText.references));
		return typeText.text;
	}
	const declarations: Array<InspectedServerDeclaration> = [];
	for (const loader of contract.viewLoaders) {
		const error = registerTypeText(loader.types.error);
		declarations.push({
			kind: "ViewLoader",
			pattern: loader.pattern,
			location: loader.location,
			input: registerTypeText(loader.types.input),
			output: registerTypeText(loader.types.output),
			errors: error === undefined ? [] : [{ typeText: error }],
		});
	}
	for (const endpoint of contract.apiEndpoints) {
		const input = registerTypeText(endpoint.types.input);
		const error = registerTypeText(endpoint.types.error);
		declarations.push({
			kind: "Endpoint",
			operation: endpoint.apiEndpointKind,
			method: endpoint.method,
			pattern: endpoint.pattern,
			location: endpoint.location,
			input,
			inputTransport:
				endpoint.apiEndpointInputKind === "form-data"
					? "formData"
					: endpoint.apiEndpointInputKind,
			output: registerTypeText(endpoint.types.output),
			errors: error === undefined ? [] : [{ typeText: error }],
		});
	}
	const typeDeclarations: TypeDeclarations = {
		declarations: contract.typeDeclarations.map((declaration) => {
			return {
				name: declaration.name,
				declaration: declaration.declaration.trimEnd(),
				referencedTypeNames: new Set(declaration.references),
			};
		}),
		typeTexts,
	};
	return {
		declarationsHash: contract.declarationsHash,
		serverDeclarations: declarations.sort(
			Order.mapInput(Order.String, (declaration) => {
				return JSON.stringify([
					declaration.kind,
					declaration.pattern,
					declaration.kind === "Endpoint" ? declaration.method : "",
				]);
			}),
		),
		typeDeclarations,
	};
}
