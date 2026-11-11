import { defineConfig } from "vorma";

export default defineConfig({
	core: {
		importMetaUrl: import.meta.url,
		distDir: "dist",
		staticPublicAssets: { srcDir: "../public", serveUrlPathPrefix: "/" },
	},
	build: {
		serverStartCmd: [
			"cargo",
			"run",
			"--manifest-path",
			"../server/Cargo.toml",
		],
	},
	client: {
		tsConfigFile: "client/tsconfig.json",
		viteConfigFile: "client/vite.config.ts",
		tsGenOutFile: "client/vorma.gen.ts",
		criticalCssFile: "client/critical.css",
		entryFile: "client/main.tsx",
		viewFilePattern: "client/**/*.v.tsx",
	},
	dev: {
		server: {
			cargoManifestFile: "../server/Cargo.toml",
			cargoBin: "vorma-renderer-example",
			preferredPort: 8081,
		},
	},
});
