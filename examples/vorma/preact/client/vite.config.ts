import prefresh from "@prefresh/vite";
import { defineConfig } from "vite";
import { createVormaPlugin } from "vorma/vite";
import config from "../vorma.config.ts";

export default defineConfig({
	// Prefresh 4 requires an explicit automatic JSX runtime for Preact.
	oxc: { jsx: { runtime: "automatic", importSource: "preact" } },
	plugins: [createVormaPlugin({ config }), await prefresh()],
});
