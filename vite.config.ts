import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";
import { fileURLToPath } from "node:url";

const host = process.env.TAURI_DEV_HOST;

const __dirname = path.dirname(fileURLToPath(import.meta.url));

export default defineConfig(({ mode }) => {
  const stub = process.env.VITE_TAURI_STUB === "1";
  return {
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "desktop"),
      ...(stub
              ? {
                  "@tauri-apps/api/core": path.resolve(__dirname, "desktop/dev/tauri-stub.ts"),
                  "@tauri-apps/api/window": path.resolve(__dirname, "desktop/dev/tauri-window-stub.ts"),
                  "@tauri-apps/api/event": path.resolve(__dirname, "desktop/dev/tauri-event-stub.ts"),
                }
              : {}),
    },
  },
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1420,
        }
      : undefined,
    watch: {
      ignored: ["**/src-tauri/**", "**/opencode/**", "**/target/**", "**/node_modules/**"],
    },
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari13",
    minify: !process.env.TAURI_ENV_DEBUG ? "esbuild" : false,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
  },
  };
});