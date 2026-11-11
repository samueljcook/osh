import {
	baseTsdownConfig,
	getTsdownEntriesFromManifest,
} from "@sjcx/repo-tooling/tsdown";
import type { UserConfig } from "tsdown";

export default {
	...baseTsdownConfig,
	entry: getTsdownEntriesFromManifest({ packageDir: import.meta.dirname }),
} satisfies UserConfig;
