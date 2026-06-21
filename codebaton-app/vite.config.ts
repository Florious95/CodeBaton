import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri expects a fixed dev server port and ignores the Rust src dir.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**", "**/src/**/*.rs", "**/target/**"],
    },
  },
  build: {
    outDir: "dist",
    target: "es2021",
    sourcemap: false,
  },
});
