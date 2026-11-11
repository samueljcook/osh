import type { OxfmtConfig } from "oxfmt";
import { BASE_IGNORE_PATTERNS } from "../shared.ts";

/** Shared formatter settings. Consumers must install the `oxfmt` peer as a development dependency. */
export const baseOxfmtConfig: OxfmtConfig = {
	ignorePatterns: BASE_IGNORE_PATTERNS,
	useTabs: true,
	tabWidth: 4,
	proseWrap: "always",
	printWidth: 80,
	sortImports: { newlinesBetween: false },
	sortPackageJson: false,
	overrides: [{ files: ["*.jsonc"], options: { trailingComma: "none" } }],
};
