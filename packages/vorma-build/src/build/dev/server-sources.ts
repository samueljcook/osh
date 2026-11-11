import { Effect, FileSystem, Path } from "effect";
import { normalizePath } from "vite";
import type { InspectionIssues } from "../inspection/inspect-projects.ts";
import { describeCause } from "../issues.ts";
import type { CargoMetadata } from "./cargo-metadata.ts";

/**
 * The files that trigger a Rust server rebuild when they change: every file that compiling
 * the binary read outside Cargo's registry, which Cargo lists in the
 * dependency file beside the executable, plus the files that decide what Cargo
 * compiles, which the dependency file omits: the server's manifest and the
 * workspace's root manifest and lockfile.
 */
export function readServerSourceFiles(input: {
	readonly executable: string;
	readonly cargoManifestFilePath: string;
	readonly cargo: CargoMetadata;
}) {
	return Effect.gen(function* () {
		const fileSystem = yield* FileSystem.FileSystem;
		const path = yield* Path.Path;
		const parsed = path.parse(input.executable);
		const dependencyFile = path.join(parsed.dir, `${parsed.name}.d`);
		function fail(problem: string) {
			return Effect.fail({
				_tag: "InspectionIssues",
				issues: [
					{
						msg: `Vorma could not read Cargo's dependency file ${dependencyFile}: ${problem}`,
					},
				],
			} satisfies InspectionIssues);
		}
		const text = yield* fileSystem
			.readFileString(dependencyFile)
			.pipe(Effect.catch((cause) => fail(describeCause(cause))));
		const prerequisites = parseDependencyFile(text);
		if (prerequisites === undefined) {
			return yield* fail("it does not contain a dependency rule");
		}
		return new Set(
			[
				...prerequisites,
				input.cargoManifestFilePath,
				path.join(input.cargo.workspaceRoot, "Cargo.toml"),
				path.join(input.cargo.workspaceRoot, "Cargo.lock"),
			].map(normalizePath),
		);
	});
}

/**
 * The prerequisites of the first rule of a Makefile-syntax dependency file,
 * or `undefined` when the file has no rule. Cargo escapes a space in a path
 * with a backslash.
 */
function parseDependencyFile(text: string): ReadonlyArray<string> | undefined {
	const rule = text
		.replaceAll("\\\n", " ")
		.split("\n")
		.find((line) => line.trim() !== "" && !line.startsWith("#"));
	const separator = rule?.search(/(?<!\\):(?:\s|$)/) ?? -1;
	if (rule === undefined || separator === -1) {
		return undefined;
	}
	const prerequisites = rule.slice(separator + 1);
	const files: Array<string> = [];
	let current = "";
	for (let index = 0; index < prerequisites.length; index++) {
		const character = prerequisites[index];
		if (character === "\\" && prerequisites[index + 1] === " ") {
			current += " ";
			index++;
		} else if (character === " " || character === "\t") {
			if (current !== "") {
				files.push(current);
				current = "";
			}
		} else {
			current += character;
		}
	}
	if (current !== "") {
		files.push(current);
	}
	return files;
}
