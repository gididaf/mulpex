import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Tauri expects a fixed dev port and does its own console handling.
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // Don't reload the frontend when Rust sources change.
      ignored: ["**/src-tauri/**", "**/crates/**", "**/target/**"],
    },
  },
});
