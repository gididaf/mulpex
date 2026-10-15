import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Tauri expects a fixed dev port and does its own console handling. The port is
// 1420 unless `npm run tauri:dev` (scripts/tauri-dev.sh) picked a free one, so
// worktrees can each run a dev app at once.
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: {
    port: Number(process.env.MULPEX_DEV_PORT) || 1420,
    strictPort: true,
    watch: {
      // Don't reload the frontend when Rust sources change.
      ignored: ["**/src-tauri/**", "**/crates/**", "**/target/**"],
    },
  },
});
