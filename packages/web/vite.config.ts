import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));

export default defineConfig({
  plugins: [react()],
  // Scan/prove workers and wasm-bindgen-rayon load split ES modules.
  worker: { format: "es" },
  server: {
    host: true,
    port: 5174,
    strictPort: true,
    headers: {
      // SharedArrayBuffer / wasm-bindgen-rayon. credentialless so Google Fonts
      // (and iOS Safari) can still isolate; require-corp often drops SAB.
      "Cross-Origin-Opener-Policy": "same-origin",
      "Cross-Origin-Embedder-Policy": "credentialless",
    },
    fs: {
      allow: [root, resolve(root, "../sdk"), resolve(root, "../core"), resolve(root, "../passkey")],
    },
  },
  preview: {
    port: 5174,
    strictPort: true,
    headers: {
      "Cross-Origin-Opener-Policy": "same-origin",
      "Cross-Origin-Embedder-Policy": "credentialless",
    },
  },
  resolve: {
    // Exact matches: "@z-stack/sdk" must not swallow "@z-stack/sdk/lab".
    alias: [
      { find: /^@z-stack\/sdk$/, replacement: resolve(root, "../sdk/src/index.ts") },
      { find: /^@z-stack\/sdk\/lab$/, replacement: resolve(root, "../sdk/src/lab.ts") },
      { find: /^@z-stack\/core$/, replacement: resolve(root, "../core/src/index.ts") },
      { find: /^@z-stack\/passkey$/, replacement: resolve(root, "../passkey/src/index.ts") },
    ],
  },
  optimizeDeps: {
    exclude: ["@z-stack/sdk"],
  },
});
