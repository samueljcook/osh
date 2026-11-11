import type { UserConfig } from "tsdown";
import {
	baseTsdownConfig,
	getTsdownEntriesFromManifest,
} from "./src/tsdown/index.ts";

export default {
	...baseTsdownConfig,
	copy: ["src/tsconfig.base.json", "src/tsconfig.package.json"],
	entry: getTsdownEntriesFromManifest({
		packageDir: import.meta.dirname,
	}),
} satisfies UserConfig;
