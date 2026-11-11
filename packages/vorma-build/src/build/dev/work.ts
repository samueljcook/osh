import { Effect, Path } from "effect";
import { testPathContained } from "primitives-ts/fs/path-containment/node";
import { normalizePath } from "vite";
import { testIgnoredStaticPublicAssetName } from "../assets/static-public-assets.ts";
import { readClientProjectRoots } from "../inspection/typescript-projects.ts";
import type { DevLayout } from "./generated-paths.ts";
import type { DevChangeRules } from "./patterns.ts";
import type { DevSourceInputs } from "./sources.ts";

/**
 * The updates that watcher batches ask for, one field per kind of update. An
 * update of one kind replaces the earlier requests of its kind. A server
 * update also does the work of the client and static updates.
 */
export type DevWork = {
	/**
	 * A Rust source of the server, a Cargo manifest, the Cargo lockfile, or a
	 * file that a `"restart-server"` rule names changed. The update builds and
	 * inspects the server, stops it while it does, and starts it again.
	 */
	readonly serverWork: boolean;
	/**
	 * A client source, or a configuration file of the client project, changed.
	 * The update inspects the client project again and reuses the server's
	 * inspection.
	 */
	readonly clientWork: boolean;
	/**
	 * A file of the public source directory or of the critical CSS changed, or
	 * a `dev.onChange` rule that requests no server restart matched a changed
	 * file. The update captures the static public assets and prepares the critical CSS.
	 */
	readonly staticWork: StaticWork | undefined;
	/**
	 * The Vite configuration, a file that it imports, an env file that Vite
	 * reads, or a file that decides how client modules resolve, such as
	 * `tsconfig.json`, changed. The update restarts Vite, and then prepares the
	 * critical CSS and the document parts with the restarted Vite's plugins.
	 */
	readonly viteRestart: boolean;
};

type StaticWork = {
	/** A matching rule requests `"revalidate-client-data"`. */
	readonly revalidatesClientData: boolean;
	/** A matching rule requests `"hard-browser-reload"`. */
	readonly reloadsBrowser: boolean;
};

export const NO_DEV_WORK: DevWork = {
	serverWork: false,
	clientWork: false,
	staticWork: undefined,
	viteRestart: false,
};

export const SERVER_WORK: DevWork = { ...NO_DEV_WORK, serverWork: true };

/** Static work that asks for neither a revalidation nor a reload. */
export const STATIC_WORK: DevWork = {
	...NO_DEV_WORK,
	staticWork: { revalidatesClientData: false, reloadsBrowser: false },
};

const CLIENT_SOURCE_WORK: DevWork = { ...NO_DEV_WORK, clientWork: true };

const VITE_RESTART_WORK: DevWork = { ...NO_DEV_WORK, viteRestart: true };

export function testDevWorkRequested(work: DevWork): boolean {
	return (
		work.serverWork ||
		work.clientWork ||
		work.staticWork !== undefined ||
		work.viteRestart
	);
}

/** Merges every requested kind of work. */
export function mergeDevWork({
	left: first,
	right: second,
}: {
	readonly left: DevWork;
	readonly right: DevWork;
}): DevWork {
	return {
		serverWork: first.serverWork || second.serverWork,
		clientWork: first.clientWork || second.clientWork,
		staticWork:
			first.staticWork === undefined && second.staticWork === undefined
				? undefined
				: {
						revalidatesClientData:
							first.staticWork?.revalidatesClientData === true ||
							second.staticWork?.revalidatesClientData === true,
						reloadsBrowser:
							first.staticWork?.reloadsBrowser === true ||
							second.staticWork?.reloadsBrowser === true,
					},
		viteRestart: first.viteRestart || second.viteRestart,
	};
}

/** What the latest prepared build read, which classifies the next batch. */
export type DevInputs = {
	readonly sources: DevSourceInputs;
	readonly configFiles: ReadonlySet<string>;
	readonly criticalCssFiles: ReadonlySet<string>;
};

/**
 * Sorts the changed files of one batch into the updates that they ask for,
 * and reports whether the batch changed the Vorma configuration or a file that
 * it imports, which restarts the session. Ordinary browser modules remain
 * Vite's hot update inputs. A batch that asks for no work asks for nothing.
 */
export function classifyDevChanges(input: {
	readonly layout: DevLayout;
	readonly inputs: DevInputs;
	/** The Vite configuration file, the files that it imports, and Vite's env files, which Vorma's restart of Vite reads again. */
	readonly viteRestartFiles: ReadonlySet<string>;
	/** The client files with a pending inspection request, which ask for client work whatever else they are. */
	readonly inspectionRequestedFiles: ReadonlySet<string>;
	readonly changedPaths: ReadonlySet<string>;
	readonly changeRules: DevChangeRules;
}) {
	return Effect.gen(function* () {
		const path = yield* Path.Path;
		const { layout, inputs, changeRules } = input;
		// Changed paths are in the form of Vite's `normalizePath`, so the paths
		// that they are compared with are too.
		const configFileId = normalizePath(layout.configFilePath);
		const criticalCssEntryId =
			layout.criticalCssEntryFilePath === undefined
				? undefined
				: normalizePath(layout.criticalCssEntryFilePath);
		function testGeneratedPath(filePath: string) {
			return layout.generatedPaths.some((dir) => {
				return testPathContained({ path, dir, filePath });
			});
		}
		let restartsSession = false;
		let work = NO_DEV_WORK;
		let checkProjectMembership = false;
		for (const filePath of [...input.changedPaths]
			.map(normalizePath)
			.filter((changedPath) => !testGeneratedPath(changedPath))) {
			if (inputs.configFiles.has(filePath) || filePath === configFileId) {
				restartsSession = true;
				continue;
			}
			const viteRestartFile = input.viteRestartFiles.has(filePath);
			const serverSource = inputs.sources.serverFiles.has(filePath);
			const clientSource =
				inputs.sources.clientFiles.has(filePath) ||
				inputs.sources.clientRootFiles.has(filePath) ||
				input.inspectionRequestedFiles.has(filePath);
			const resolutionSource =
				inputs.sources.resolutionFiles.has(filePath);
			const actions = changeRules.getFileActions(
				path.relative(layout.configDir, filePath),
			);
			if (serverSource || actions.includes("restart-server")) {
				work = mergeDevWork({ left: work, right: SERVER_WORK });
			}
			if (clientSource || resolutionSource) {
				work = mergeDevWork({ left: work, right: CLIENT_SOURCE_WORK });
			}
			if (viteRestartFile || resolutionSource) {
				work = mergeDevWork({ left: work, right: VITE_RESTART_WORK });
			}
			const staticFile =
				(testPathContained({
					path,
					dir: layout.staticPublicAssetsSourceDir,
					filePath,
				}) &&
					!testIgnoredStaticPublicAssetName(
						path.basename(filePath),
					)) ||
				inputs.criticalCssFiles.has(filePath) ||
				filePath === criticalCssEntryId;
			const revalidatesClientData = actions.includes(
				"revalidate-client-data",
			);
			const reloadsBrowser = actions.includes("hard-browser-reload");
			if (staticFile || revalidatesClientData || reloadsBrowser) {
				work = mergeDevWork({
					left: work,
					right: {
						...NO_DEV_WORK,
						staticWork: { revalidatesClientData, reloadsBrowser },
					},
				});
			}
			checkProjectMembership ||=
				!serverSource &&
				!clientSource &&
				!resolutionSource &&
				!viteRestartFile;
		}
		// A new Rust source joins the binary only through a change to a source
		// that the binary already reads, so only client membership needs a check.
		if (
			checkProjectMembership &&
			!restartsSession &&
			!work.serverWork &&
			!work.clientWork
		) {
			const roots = yield* readClientProjectRoots(layout);
			const next = new Set(
				roots
					.map(normalizePath)
					.filter((file) => !testGeneratedPath(file)),
			);
			const previous = [...inputs.sources.clientRootFiles].filter(
				(file) => !testGeneratedPath(file),
			);
			if (
				previous.length !== next.size ||
				previous.some((file) => !next.has(file))
			) {
				work = mergeDevWork({ left: work, right: CLIENT_SOURCE_WORK });
			}
		}
		return { restartsSession, work };
	});
}
