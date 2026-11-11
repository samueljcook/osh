import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";
import { createVormaPlugin } from "vorma/vite";
import config from "../vorma.config.ts";

export default defineConfig({
	plugins: [createVormaPlugin({ config }), react()],
});
