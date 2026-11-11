import type { Server } from "node:http";
import type { UserConfig } from "vite";
import { ARTIFACT_PROTOCOL } from "../output/dist-dir.ts";

/*
The Vite options that Vorma applies in its own Vite invocations, and the check
that reports the options of an application's Vite configuration that have no
effect there. The check derives the options that Vorma sets from the same
functions that produce its options, so the two cannot drift apart.
*/

/** The loopback address of Vorma's module server, from which development documents load browser modules. */
export const MODULE_SERVER_HOST = "127.0.0.1";

/**
 * Vite runs in middleware mode inside Vorma's module server: it serves browser
 * modules and hot updates on that server's port, and its WebSocket connection
 * uses the same server.
 */
export function createDevViteOptions(input: {
	/** `http://127.0.0.1:<port>`. */
	readonly origin: string;
	readonly port: number;
	readonly server: Server;
}) {
	return {
		base: "/",
		publicDir: false,
		appType: "custom",
		server: {
			middlewareMode: true,
			origin: input.origin,
			ws: {
				server: input.server,
				protocol: "ws",
				host: MODULE_SERVER_HOST,
				port: input.port,
				clientPort: input.port,
			},
		},
	} satisfies UserConfig;
}

/** A production build serves its hashed files at the static public assets path and writes no Vite public directory. */
export function createBuildViteOptions(input: { readonly base: string }) {
	return { base: input.base, publicDir: false } satisfies UserConfig;
}

/**
 * The client environment builds exactly `inputs`, the client entry, the view
 * modules, and the critical CSS entry, and writes ES modules into the build
 * directory's public directory, which Vorma clears itself. The client output
 * hashing plugin names every output with
 * `ARTIFACT_PROTOCOL.clientOutputFileNames`.
 */
export function createClientBuildViteOptions(input: {
	readonly outDir: string;
	/** The source files that the build starts from. Rolldown names each output after its file. */
	readonly inputs: ReadonlyArray<string>;
}) {
	return {
		build: {
			outDir: input.outDir,
			emptyOutDir: false,
			write: true,
			assetsDir: ".",
			cssCodeSplit: true,
			rolldownOptions: {
				input: [...input.inputs],
				preserveEntrySignatures: "strict",
				output: {
					format: "es",
					dir: input.outDir,
				},
			},
		},
	} satisfies UserConfig;
}

/** Options that apply only when Vite owns a server or performs server-side rendering, with the reason that each has no effect. */
const SERVER_ONLY_OPTIONS: ReadonlyArray<{
	readonly option: string;
	readonly msg: string;
}> = [
	{
		option: "server.port",
		msg: "which chooses the module server's port itself",
	},
	{
		option: "server.strictPort",
		msg: "which chooses the module server's port itself",
	},
	{
		option: "server.host",
		msg: "which chooses the module server's address itself",
	},
	{
		option: "server.open",
		msg: "because Vite runs in middleware mode and never opens a browser",
	},
	{
		option: "server.https",
		msg: "because Vite runs in middleware mode and never serves HTTPS",
	},
	{ option: "preview", msg: "which never runs Vite's preview server" },
	{
		option: "ssr",
		msg: "which never uses Vite for server-side rendering",
	},
];

/** Every option path in an options object; an empty object is one option's value. */
function listOptionPaths(options: object, prefix = ""): Array<string> {
	return Object.entries(options).flatMap(([key, value]) => {
		const path = `${prefix}${key}`;
		return testPlainObject(value) && Object.keys(value).length > 0
			? listOptionPaths(value, `${path}.`)
			: [path];
	});
}

function testPlainObject(value: unknown): value is object {
	if (typeof value !== "object" || value === null) {
		return false;
	}
	const prototype = Object.getPrototypeOf(value);
	return prototype === Object.prototype || prototype === null;
}

type AuthoredOption = { readonly path: string; readonly value: unknown };

/** The options that an authored Vite configuration sets; an empty object is one option's value. */
function listAuthoredOptions(
	config: object,
	prefix = "",
): Array<AuthoredOption> {
	return Object.entries(config).flatMap(([key, value]) => {
		if (value === undefined) {
			return [];
		}
		const path = `${prefix}${key}`;
		return testPlainObject(value) && Object.keys(value).length > 0
			? listAuthoredOptions(value, `${path}.`)
			: [{ path, value }];
	});
}

/**
 * The options that Vorma owns: the plugin's `root`, the options of
 * development and the production build, the client environment's build
 * options, which include the build's inputs, and the names of its outputs.
 */
function listVormaOwnedOptions(): ReadonlyArray<string> {
	return [
		"root",
		...Object.keys(ARTIFACT_PROTOCOL.clientOutputFileNames).map((name) => {
			return `build.rolldownOptions.output.${name}`;
		}),
		...listOptionPaths(
			createDevViteOptions({
				origin: "",
				port: 0,
				// Only the option names matter here.
				server: {} as Server,
			}),
		),
		...listOptionPaths(createBuildViteOptions({ base: "" })),
		...listOptionPaths(
			createClientBuildViteOptions({ outDir: "", inputs: [] }),
		),
	];
}

/**
 * Other names that Vite accepts for the options that Vorma sets: the
 * deprecated `server.hmr` names of the WebSocket connection, the client
 * environment's own build options, and `rollupOptions` for `rolldownOptions`.
 */
function listAliases(options: ReadonlyArray<string>): ReadonlyArray<string> {
	return options.flatMap((option) => {
		if (option.startsWith("server.ws.")) {
			return [option.replace("server.ws.", "server.hmr.")];
		}
		if (!option.startsWith("build.")) {
			return [];
		}
		const envOption = `environments.client.${option}`;
		return [
			envOption,
			...[option, envOption].flatMap((buildOption) => {
				return buildOption.includes(".rolldownOptions.")
					? [
							buildOption.replace(
								".rolldownOptions.",
								".rollupOptions.",
							),
						]
					: [];
			}),
		];
	});
}

const VORMA_OWNED_OPTIONS = listVormaOwnedOptions();
const VORMA_OWNED_OPTION_ALIASES = listAliases(VORMA_OWNED_OPTIONS);

/** Whether an authored option is the option or is inside it. */
function testOptionCovered({
	authoredPath,
	option,
}: {
	authoredPath: string;
	option: string;
}): boolean {
	return authoredPath === option || authoredPath.startsWith(`${option}.`);
}

/**
 * One message for each option of an authored Vite configuration that has no
 * effect under Vorma: an option that Vorma sets itself, and an option that
 * applies only when Vite owns a server or performs server-side rendering.
 * Options that act on Vite's module serving in middleware mode, such as
 * `server.cors`, `server.fs`, `server.proxy`, and `server.hmr.overlay`, are
 * not reported.
 */
export function findViteOptionsWithoutEffect(input: {
	/** The Vite configuration as its file exports it, before plugins apply. */
	readonly config: object;
	/** How messages name the Vite configuration file. */
	readonly fileName: string;
}): ReadonlyArray<string> {
	const reported = new Map<string, string>();
	for (const authored of listAuthoredOptions(input.config)) {
		const option =
			[...VORMA_OWNED_OPTIONS, ...VORMA_OWNED_OPTION_ALIASES].find(
				(ownedOption) => {
					return testOptionCovered({
						authoredPath: authored.path,
						option: ownedOption,
					});
				},
			) ??
			// A boolean that replaces a group that Vorma sets, such as
			// `server.ws: false`, is reported under its own name.
			(typeof authored.value === "boolean" &&
			VORMA_OWNED_OPTIONS.some((ownedOption) => {
				return ownedOption.startsWith(`${authored.path}.`);
			})
				? authored.path
				: undefined);
		if (option !== undefined) {
			reported.set(option, "which sets it itself");
			continue;
		}
		const serverOnly = SERVER_ONLY_OPTIONS.find((entry) => {
			return testOptionCovered({
				authoredPath: authored.path,
				option: entry.option,
			});
		});
		if (serverOnly !== undefined) {
			reported.set(serverOnly.option, serverOnly.msg);
		}
	}
	return [...reported].map(([reportedOption, msg]) => {
		return `${input.fileName}: ${reportedOption} has no effect under Vorma, ${msg}. Remove it, or move it to the configuration of the tool that uses it.`;
	});
}
