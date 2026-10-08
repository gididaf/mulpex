import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { fileURLToPath } from "node:url";

// The phone web app for Remote Control. Built to `remote/dist`, which
// `mulpex-relay` serves. Shares the app's node_modules; nothing here is bundled
// into the desktop app.
export default defineConfig({
  root: fileURLToPath(new URL(".", import.meta.url)),
  base: "/",
  plugins: [svelte()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  server: {
    port: 1430,
    proxy: { "/ws": { target: "ws://127.0.0.1:8787", ws: true } },
  },
});
