import { posix } from "node:path";
import { createFilter, normalizePath } from "vite";
import type { DevAction, VormaConfig } from "../../config.ts";

export type DevChangeRules = ReturnType<typeof compileDevChangeRules>;

type DevOnChangeRule = NonNullable<
	NonNullable<VormaConfig["dev"]>["onChange"]
>[number];

/** A `dev.onChange` pattern in the form that matching uses. */
type NormalizedDevPattern = {
	/** The pattern starts with `!` and removes the files that it matches. */
	readonly isExcluded: boolean;
	/** The pattern ends with `/` and matches only directories and their contents. */
	readonly isDirOnly: boolean;
	/**
	 * The glob relative to the configuration directory, or `"."` for every
	 * file. It is empty when the authored pattern names no file, as `"/"`,
	 * `"!"`, and `""` do.
	 */
	readonly glob: string;
};

/**
 * Normalizes one authored pattern. A pattern without a slash matches at any
 * depth; a pattern that starts with `/` or `./` is anchored at the
 * configuration directory.
 */
function normalizeDevPattern(raw: string): NormalizedDevPattern {
	const trimmed = raw.trim();
	const isExcluded = trimmed.startsWith("!");
	const source = isExcluded ? trimmed.slice(1).trim() : trimmed;
	if (source === "") {
		return { isExcluded, isDirOnly: false, glob: "" };
	}
	const anchored = source.startsWith("/") || source.startsWith("./");
	const normalized = normalizePath(source);
	const isDirOnly = normalized.endsWith("/");
	const relative = normalized
		.replace(/\/$/, "")
		.replace(/^\.\//, "")
		.replace(/^\//, "");
	return {
		isExcluded,
		isDirOnly,
		glob:
			anchored ||
			relative === "" ||
			relative === "." ||
			relative.includes("/")
				? relative
				: `**/${relative}`,
	};
}

/**
 * The messages for the `dev.onChange` rules that cannot work: a pattern that
 * names no file once normalized, such as `""`, `"!"`, or `"/"`, and a rule
 * with only exclusion patterns, which match nothing.
 */
export function findInvalidDevChangeRules(
	rules: ReadonlyArray<DevOnChangeRule>,
): ReadonlyArray<string> {
	return rules.flatMap((rule, index) => {
		const name = `dev.onChange[${index}]`;
		const normalized = rule.triggers.map((pattern) => {
			return { pattern, normalized: normalizeDevPattern(pattern) };
		});
		const emptyPattern = normalized.find((entry) => {
			return entry.normalized.glob === "";
		});
		if (emptyPattern !== undefined) {
			return [
				`${name}: the pattern ${JSON.stringify(emptyPattern.pattern)} names no file. Name files relative to the configuration directory, or use "./" for every file.`,
			];
		}
		return normalized.every((entry) => entry.normalized.isExcluded)
			? [
					`${name}: every pattern is an exclusion, so the rule matches no file. Add a pattern that selects files.`,
				]
			: [];
	});
}

/**
 * Ordered inclusion and exclusion rules use native Vite glob matching. The
 * configuration loader rejects every pattern that `findInvalidDevChangeRules`
 * reports, so each pattern here names files.
 */
export function compileDevPatterns(patterns: ReadonlyArray<string>) {
	const rules = patterns.map((raw) => {
		const {
			isExcluded,
			isDirOnly,
			glob: pattern,
		} = normalizeDevPattern(raw);
		const testPathMatched = createFilter(pattern, undefined, {
			resolve: false,
		});
		const segments = pattern.split("/");
		const prefix = segments.filter((_segment, index) => {
			return segments
				.slice(0, index + 1)
				.every((segment) => !/[*?{[]/.test(segment));
		});
		// A wholly literal pattern names a file, so the file's directory is the root;
		// otherwise the literal prefix is the root.
		const rootSegments =
			prefix.length === segments.length ? prefix.slice(0, -1) : prefix;
		return {
			pattern,
			isDirOnly,
			isExcluded,
			testPathMatched,
			root: rootSegments.join("/") || ".",
		};
	});
	return {
		roots: rules
			.filter((rule) => !rule.isExcluded)
			.map((rule) => rule.root),
		testRelativePathMatched(relativePath: string) {
			const segments = posix
				.normalize(normalizePath(relativePath))
				.split("/");
			let matched = false;
			for (const rule of rules) {
				if (
					rule.pattern === "." ||
					segments.some((_segment, index) => {
						return (
							(!rule.isDirOnly || index < segments.length - 1) &&
							rule.testPathMatched(
								segments.slice(0, index + 1).join("/"),
							)
						);
					})
				) {
					matched = !rule.isExcluded;
				}
			}
			return matched;
		},
	};
}

/**
 * Compiles the `dev.onChange` rules. A rule applies to a changed file that one
 * of its patterns matches; the patterns are relative to the configuration
 * directory.
 */
export function compileDevChangeRules(rules: ReadonlyArray<DevOnChangeRule>) {
	const compiled = rules.map((rule) => {
		return {
			actions: rule.actions,
			files: compileDevPatterns(rule.triggers),
		};
	});
	return {
		/** The directories that the patterns can match below. */
		roots: compiled.flatMap((rule) => rule.files.roots),
		/** The actions of the rules with a pattern that matches a changed file. */
		getFileActions(relativePath: string): ReadonlyArray<DevAction> {
			return compiled.flatMap((rule) => {
				return rule.files.testRelativePathMatched(relativePath)
					? rule.actions
					: [];
			});
		},
	};
}
