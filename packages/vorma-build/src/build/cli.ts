#!/usr/bin/env node
import { createRequire } from "node:module";
import {
	NodeChildProcessSpawner,
	NodeCrypto,
	NodeFileSystem,
	NodePath,
	NodeRuntime,
	NodeStdio,
	NodeTerminal,
} from "@effect/platform-node-shared";
import {
	Console,
	Deferred,
	Effect,
	FileSystem,
	Layer,
	Runtime,
	Schema,
} from "effect";
import { Argument, Command } from "effect/cli";
import { checkClientPackageVersion } from "./client-package.ts";
import { loadVormaConfig } from "./config/load-config.ts";
import {
	printBuildIssues,
	readBuildIssues,
	toBuildIssues,
	type VormaBuildIssues,
} from "./issues.ts";

const packageJson = Schema.decodeUnknownSync(
	Schema.Struct({ version: Schema.NonEmptyString }),
)(createRequire(import.meta.url)("../../package.json"));

const configArg = Argument.File("config", { mustExist: true }).pipe(
	Argument.withDescription(
		"Path to the application's Vorma configuration file.",
	),
);

/**
 * Loads a command's module after the check that the installed `@vorma/client`
 * has this package's version. The modules of the commands import
 * `@vorma/client`, so the check reports a missing or mismatched
 * `@vorma/client` before Node resolves those imports.
 */
function loadCmdModule<Module>(
	load: () => Promise<Module>,
): Effect.Effect<Module, VormaBuildIssues, FileSystem.FileSystem> {
	return checkClientPackageVersion({
		buildPackageVersion: packageJson.version,
		resolutionBaseUrl: import.meta.url,
	}).pipe(
		Effect.mapError(toBuildIssues),
		Effect.andThen(Effect.promise(load)),
	);
}

const buildCmd = Command.make("build", { config: configArg }, ({ config }) => {
	return Effect.gen(function* () {
		const { buildVormaApplication } = yield* loadCmdModule(
			() => import("./build-application.ts"),
		);
		const loaded = yield* loadVormaConfig({
			configPath: config,
			cmd: "build",
			mode: "production",
		}).pipe(Effect.mapError(toBuildIssues));
		const result = yield* buildVormaApplication({
			resolved: loaded.resolved,
		});
		yield* Console.log(
			`Published Vorma build ${result.clientBuildId} to ${loaded.resolved.buildDir}`,
		);
	}).pipe(
		Effect.scoped,
		Effect.catchTag("VormaBuildIssues", reportBuildIssues),
	);
}).pipe(
	Command.withDescription(
		"Build and publish production application artifacts.",
	),
);

const nodeServices = Layer.provideMerge(
	NodeChildProcessSpawner.layer,
	Layer.mergeAll(
		NodeFileSystem.layer,
		NodeCrypto.layer,
		NodePath.layer,
		NodeStdio.layer,
		NodeTerminal.layer,
	),
);

/** Prints every build issue and ends the command with status 1, without internal structures. */
function reportBuildIssues(issues: VormaBuildIssues): Effect.Effect<void> {
	return printBuildIssues(issues).pipe(
		Effect.andThen(
			Effect.sync(() => {
				process.exitCode = 1;
			}),
		),
	);
}

const devCmd = Command.make("dev", { config: configArg }, ({ config }) => {
	return loadCmdModule(() => import("./dev/session.ts")).pipe(
		Effect.flatMap(({ developApplication }) => developApplication(config)),
		Effect.catch((failure) => {
			const issues = readBuildIssues(failure);
			return issues === undefined
				? Effect.fail(failure)
				: reportBuildIssues(issues);
		}),
	);
}).pipe(
	Command.withDescription(
		"Run application development with rebuilds and browser hot updates.",
	),
);

/** The signals that end a command, with the conventional exit status of each: 128 and the signal's number. */
const TERMINATION_EXIT_CODES = {
	SIGHUP: 129,
	SIGINT: 130,
	SIGTERM: 143,
} as const;

type TerminationSignal = keyof typeof TERMINATION_EXIT_CODES;

/** Whether a signal asked the command to end, which decides the exit status. */
let termination:
	| { readonly kind: "running" }
	| { readonly kind: "signaled"; readonly signal: TerminationSignal } = {
	kind: "running",
};

/**
 * Records the first termination signal until the command's cleanup ends.
 * `NodeRuntime.runMain` interrupts the program on `SIGINT` and `SIGTERM`.
 * Closing the terminal sends `SIGHUP`, which `runMain` does not handle, so
 * the program races `hangup`, which interrupts the program and runs its
 * cleanup. The cleanup stops the application server and releases the output
 * lock, and the process then ends with the signal's status. A second
 * `SIGINT` ends the process at once with status 130, because a cleanup can
 * wait without a limit: a Vite build cannot be interrupted, and a Vite close
 * can stall. The operating system releases the lock of a process that ends,
 * and the next build removes the staging directory that it left. The
 * listeners stay registered until the cleanup ends: `signal-exit`, which
 * Rolldown loads, raises a signal again when it finds no other listener,
 * which would end the process before the cleanup.
 */
const watchTerminationSignals = Effect.gen(function* () {
	const hangup = yield* Deferred.make<void>();
	function handleSignal(signal: TerminationSignal) {
		if (termination.kind === "signaled") {
			if (signal === "SIGINT") {
				process.exit(TERMINATION_EXIT_CODES.SIGINT);
			}
			return;
		}
		termination = { kind: "signaled", signal };
		if (signal === "SIGHUP") {
			Deferred.doneUnsafe(hangup, Effect.void);
		}
	}
	const listeners = (
		Object.keys(TERMINATION_EXIT_CODES) as ReadonlyArray<TerminationSignal>
	).map((signal) => {
		return { signal, listener: () => handleSignal(signal) };
	});
	yield* Effect.acquireRelease(
		Effect.sync(() => {
			for (const { signal, listener } of listeners) {
				process.on(signal, listener);
			}
		}),
		() => {
			return Effect.sync(() => {
				for (const { signal, listener } of listeners) {
					process.removeListener(signal, listener);
				}
			});
		},
	);
	return { hangup: Deferred.await(hangup) };
});

/**
 * An exception or a rejected promise that nothing handles ends the command with
 * status 1 after the program's cleanup, which stops the application server and
 * releases the output lock. Without a listener, Node ends the process at once
 * without that cleanup. The listeners stay registered until the cleanup
 * ends, so that another unhandled error during the cleanup does not end the
 * process before the cleanup finishes.
 */
const watchUnhandledErrors = Effect.gen(function* () {
	const unhandledError = yield* Deferred.make<never>();
	function handleUnhandledError(error: unknown) {
		Deferred.doneUnsafe(unhandledError, Effect.die(error));
	}
	yield* Effect.acquireRelease(
		Effect.sync(() => {
			process.on("uncaughtException", handleUnhandledError);
			process.on("unhandledRejection", handleUnhandledError);
		}),
		() => {
			return Effect.sync(() => {
				process.removeListener(
					"uncaughtException",
					handleUnhandledError,
				);
				process.removeListener(
					"unhandledRejection",
					handleUnhandledError,
				);
			});
		},
	);
	return Deferred.await(unhandledError);
});

const runCli = Effect.gen(function* () {
	const signals = yield* watchTerminationSignals;
	const unhandledError = yield* watchUnhandledErrors;
	return yield* Command.make("vorma").pipe(
		Command.withSubcommands([buildCmd, devCmd]),
		Command.run({ version: packageJson.version }),
		Effect.provide(nodeServices),
		Effect.raceFirst(signals.hangup),
		Effect.raceFirst(unhandledError),
	);
}).pipe(Effect.scoped);

// A command that a signal ended exits with the signal's status after its
// cleanup; `runMain` calls `process.exit` with every status other than 0.
NodeRuntime.runMain(runCli, {
	teardown: (exit, onExit) => {
		if (termination.kind === "signaled") {
			onExit(TERMINATION_EXIT_CODES[termination.signal]);
			return;
		}
		Runtime.defaultTeardown(exit, onExit);
	},
});
