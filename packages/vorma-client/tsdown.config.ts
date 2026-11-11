import {
	baseTsdownConfig,
	getTsdownEntriesFromManifest,
} from "@sjcx/repo-tooling/tsdown";
import { transform } from "@solidjs/compiler";
import type { UserConfig } from "tsdown";

export default {
	...baseTsdownConfig,
	plugins: [
		{
			name: "vorma-solid-jsx",
			transform: {
				filter: { id: /\/solid\/.*\.tsx$/ },
				handler(code, id) {
					const result = transform(code, {
						filename: id,
						generate: "dom",
						hydratable: false,
						dev: false,
					});
					return { code: result.code, map: result.map ?? null };
				},
			},
		},
	],
	entry: getTsdownEntriesFromManifest({
		packageDir: import.meta.dirname,
	}),
} satisfies UserConfig;
