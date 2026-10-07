import { defineConfig } from "vite";
export default defineConfig({ server: { strictPort: true, port: 5173,
  proxy: { "/api": { target: "http://127.0.0.1:3010", changeOrigin: false } } } });
