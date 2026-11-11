/** Vite supplies the application mode and replaces it during browser builds. */
interface ImportMetaEnv {
	readonly MODE: string;
}

interface ImportMeta {
	readonly env: ImportMetaEnv;
}
