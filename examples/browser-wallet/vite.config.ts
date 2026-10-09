import { defineConfig, type Plugin } from "vite";
import { zStack } from "@z-stack/sdk/vite";

// Preview demonstrates the production policy: immutable hashed assets, fresh HTML.
const previewCache: Plugin = {
  name: "example-preview-cache",
  configurePreviewServer(server) {
    server.middlewares.use((request, response, next) => {
      const path = new URL((request as { url?: string }).url ?? "/", "http://localhost").pathname;
      const hashedAsset = /^\/assets\/[^/]+-[A-Za-z0-9_-]{8,}\.(?:wasm|js|css)$/.test(path);
      const writeHead = response.writeHead;
      response.writeHead = function(this: typeof response, code: number, ...args: unknown[]) {
        const type = String(this.getHeader("Content-Type") ?? "");
        if (hashedAsset && (code === 200 || code === 304) && !type.includes("text/html")) {
          this.setHeader("Cache-Control", "public, max-age=31536000, immutable");
        }
        return Reflect.apply(writeHead, this, [code, ...args]);
      } as typeof response.writeHead;
      next();
    });
  },
};
export default defineConfig({ plugins: [zStack(), previewCache], build: { target: "es2022" } });
