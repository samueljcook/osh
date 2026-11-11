import { fileURLToPath } from "node:url";
import type { OxlintConfig } from "oxlint";
import { BASE_IGNORE_PATTERNS } from "../shared.ts";
import plugin, { PLUGIN_NAME } from "./plugin/index.ts";

/** Shared lint rules and type-aware checking. Consumers must install `oxlint` and `oxlint-tsgolint` as development dependencies. */
export const baseOxlintConfig: OxlintConfig = {
	ignorePatterns: BASE_IGNORE_PATTERNS,
	// The built plugin entry beside this module; a package name would resolve
	// only under the name that a consumer installs the package as.
	jsPlugins: [fileURLToPath(new URL("./plugin/index.js", import.meta.url))],
	rules: {
		...Object.fromEntries(
			Object.keys(plugin.rules).map((name): [string, "error"] => {
				return [`${PLUGIN_NAME}/${name}`, "error"];
			}),
		),
		curly: "error",
		"typescript/array-type": ["error", { default: "generic" }],
		"func-style": ["error", "declaration"],
	},
	options: { typeAware: true },
};
