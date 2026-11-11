import type { VormaConfig } from "../config.ts";

/** The application configuration exposed through Vite's plugin API. */
export type VormaPluginApi = {
	readonly config: VormaConfig;
};
