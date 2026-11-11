import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import type { UserConfig } from "tsdown";
import { DIST_DIR_NAME } from "../shared.ts";

const SOURCE_DIR_NAME = "src";
const MANIFEST_FILE_NAME = "package.json";
const ENTRY_ARTIFACT_SUFFIX = ".js";
const DECLARATION_ARTIFACT_SUFFIX = ".d.ts";
/**
 * Code file extensions that no source entry produces. The declaration
 * extensions `.d.mts` and `.d.cts` end in `.mts` and `.cts`.
 */
const UNBUILT_CODE_SUFFIXES = [
	".mjs",
	".cjs",
	".ts",
	".mts",
	".cts",
	".jsx",
	".tsx",
];

/** Shared ESM and source-map settings. Consumers must install the `tsdown` peer as a development dependency. */
export const baseTsdownConfig = {
	format: "esm",
	deps: { neverBundle: true },
	sourcemap: true,
	dts: false,
	platform: "neutral",
	root: SOURCE_DIR_NAME,
	outDir: DIST_DIR_NAME,
} satisfies UserConfig;

function collectDistArtifactPaths(value: unknown): ReadonlyArray<string> {
	if (typeof value === "string") {
		return value.startsWith(`./${DIST_DIR_NAME}/`) ? [value] : [];
	}
	if (Array.isArray(value)) {
		return value.flatMap(collectDistArtifactPaths);
	}
	if (typeof value === "object" && value !== null) {
		return Object.values(value).flatMap(collectDistArtifactPaths);
	}
	return [];
}

function testForJsonObject(
	value: unknown,
): value is Readonly<Record<string, unknown>> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Converts the exception that `JSON.parse` throws into one that names the manifest. */
function parseManifestText(input: {
	readonly manifestPath: string;
	readonly text: string;
}): unknown {
	try {
		return JSON.parse(input.text);
	} catch (cause) {
		throw new Error(`${input.manifestPath} is not valid JSON`, { cause });
	}
}

function readManifestArtifactFields(manifestPath: string): {
	readonly bin: unknown;
	readonly exports: unknown;
} {
	const parsed = parseManifestText({
		manifestPath,
		text: readFileSync(manifestPath, "utf8"),
	});
	if (!testForJsonObject(parsed)) {
		throw new Error(`${manifestPath} must contain a JSON object`);
	}
	return { bin: parsed["bin"], exports: parsed["exports"] };
}

type ArtifactClass =
	| { readonly kind: "entry" }
	| { readonly kind: "declaration" }
	| { readonly kind: "asset" }
	| { readonly kind: "invalid"; readonly reason: string };

/**
 * Classifies one `exports` or `bin` artifact under `./dist/`. A `.js` file maps
 * to a source entry. A `bin` artifact must be a `.js` file. A `.d.ts` file is a
 * declaration only when the manifest also declares the `.js` artifact beside
 * it. A file with another code extension is invalid. Any other file is an asset
 * that another build step provides.
 */
function classifyArtifact(input: {
	readonly artifactPath: string;
	readonly isBinArtifact: boolean;
	readonly declaredArtifactPaths: ReadonlySet<string>;
}): ArtifactClass {
	const { artifactPath } = input;
	if (artifactPath.endsWith(ENTRY_ARTIFACT_SUFFIX)) {
		return { kind: "entry" };
	}
	if (input.isBinArtifact) {
		return { kind: "invalid", reason: "a bin artifact must be a .js file" };
	}
	if (artifactPath.endsWith(DECLARATION_ARTIFACT_SUFFIX)) {
		const scriptPath = `${artifactPath.slice(0, -DECLARATION_ARTIFACT_SUFFIX.length)}${ENTRY_ARTIFACT_SUFFIX}`;
		return input.declaredArtifactPaths.has(scriptPath)
			? { kind: "declaration" }
			: {
					kind: "invalid",
					reason: `the manifest declares no ${scriptPath} artifact beside it`,
				};
	}
	if (
		UNBUILT_CODE_SUFFIXES.some((suffix) => {
			return artifactPath.endsWith(suffix);
		})
	) {
		return {
			kind: "invalid",
			reason: "no source entry produces this code file",
		};
	}
	return { kind: "asset" };
}

/**
 * Lists the tsdown source entries that a package manifest's `exports` and
 * `bin` artifacts require. Each `./dist/<path>.js` artifact maps to
 * `src/<path>.ts` or `src/<path>.tsx`, whichever exists. A `.d.ts` artifact is
 * accepted when the manifest also declares the `.js` artifact beside it. Other
 * files, such as `.json`, `.wasm`, and `.css` files, are assets that the build
 * configuration neither builds nor rejects. A `bin` artifact that is not a
 * `.js` file, a `.d.ts` artifact without that `.js` artifact, a file with
 * another code extension (`.mjs`, `.cjs`, `.ts`, `.mts`, `.cts`, `.jsx`,
 * `.tsx`, `.d.mts`, or `.d.cts`), and a `.js` artifact with no source entry
 * violate the package contract, so the build configuration fails to load
 * instead of producing a partial build. The error names the manifest and each
 * such artifact. A manifest that is not valid JSON or not a JSON object also
 * fails, with an error that names the manifest.
 */
export function getTsdownEntriesFromManifest(input: {
	readonly packageDir: string;
}): Array<string> {
	const manifestPath = join(input.packageDir, MANIFEST_FILE_NAME);
	const manifest = readManifestArtifactFields(manifestPath);
	const exportArtifactPaths = collectDistArtifactPaths(manifest.exports);
	const binArtifactPaths = collectDistArtifactPaths(manifest.bin);
	const declaredArtifactPaths = new Set([
		...exportArtifactPaths,
		...binArtifactPaths,
	]);
	const classifiedArtifacts = [
		...exportArtifactPaths.map((artifactPath) => {
			return { artifactPath, isBinArtifact: false };
		}),
		...binArtifactPaths.map((artifactPath) => {
			return { artifactPath, isBinArtifact: true };
		}),
	].map(({ artifactPath, isBinArtifact }) => {
		return {
			artifactPath,
			artifactClass: classifyArtifact({
				artifactPath,
				isBinArtifact,
				declaredArtifactPaths,
			}),
		};
	});
	const invalidArtifacts = new Set(
		classifiedArtifacts.flatMap(({ artifactPath, artifactClass }) => {
			return artifactClass.kind === "invalid"
				? [`${artifactPath} (${artifactClass.reason})`]
				: [];
		}),
	);
	if (invalidArtifacts.size > 0) {
		throw new Error(
			`${manifestPath} declares artifacts that its build cannot produce: ${[...invalidArtifacts].join("; ")}`,
		);
	}
	const artifactPaths = new Set(
		classifiedArtifacts.flatMap(({ artifactPath, artifactClass }) => {
			return artifactClass.kind === "entry" ? [artifactPath] : [];
		}),
	);
	return [...artifactPaths].sort().map((artifactPath) => {
		const sourceStem = artifactPath
			.replace(`./${DIST_DIR_NAME}/`, `${SOURCE_DIR_NAME}/`)
			.replace(/\.js$/, "");
		const candidates = [`${sourceStem}.ts`, `${sourceStem}.tsx`];
		const entry = candidates.find((candidate) => {
			return existsSync(join(input.packageDir, candidate));
		});
		if (entry === undefined) {
			throw new Error(
				`${manifestPath} declares ${artifactPath}, but no source entry exists at ${candidates.join(" or ")}`,
			);
		}
		return entry;
	});
}
