import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

export default defineConfig({
  plugins: [svelte()],
  // Keep Rust compiler output visible when running under `tauri dev`.
  clearScreen: false,
  server: {
    // Tauri's devUrl expects exactly this port.
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] },
  },
});
