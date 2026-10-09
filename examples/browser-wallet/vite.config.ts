import { defineConfig, type Plugin } from "vite";
import compression from "compression";
import { existsSync, readdirSync } from "node:fs";
import { resolve } from "node:path";
import { zStack } from "@z-stack/sdk/vite";

// Preview demonstrates the production policy: immutable hashed assets, fresh HTML.
const previewCache: Plugin = {
  name: "example-preview-cache",
  configurePreviewServer(server) {
    const assetsDir = resolve(server.config.root, server.config.build.outDir, server.config.build.assetsDir);
    const assets = new Set(existsSync(assetsDir) ? readdirSync(assetsDir, {withFileTypes:true})
      .filter(entry => entry.isFile()).map(entry => entry.name) : []);
    server.middlewares.use((request, response, next) => {
      const path = new URL((request as { url?: string }).url ?? "/", "http://localhost").pathname;
      const hashedAsset = assets.has(path.slice("/assets/".length)) && /^\/assets\/[^/]+-[A-Za-z0-9_-]{8,}\.(?:wasm|js|css)$/.test(path);
      const writeHead = response.writeHead;
      response.writeHead = function(this: typeof response, code: number, ...args: unknown[]) {
        // sirv supplies headers directly to writeHead for HEAD responses.
        const headerIndex = typeof args[0] === "string" ? 1 : 0;
        const headers = args[headerIndex] as Record<string, unknown> | undefined;
        const type = String(Object.entries(headers ?? {}).find(([name]) => name.toLowerCase() === "content-type")?.[1]
          ?? this.getHeader("Content-Type") ?? "");
        if (hashedAsset && (code === 200 || code === 304) && !type.includes("text/html")) {
          const policy = "public, max-age=31536000, immutable";
          this.setHeader("Cache-Control", policy);
          if (headers) {
            const updated = Object.fromEntries(Object.entries(headers).filter(([name]) => name.toLowerCase() !== "cache-control"));
            args[headerIndex] = { ...updated, "Cache-Control": policy };
          }
        }
        return Reflect.apply(writeHead, this, [code, ...args]);
      } as typeof response.writeHead;
      next();
    });
    // Vite's default compressor excludes application/wasm. Keep negotiation,
    // streaming and no-transform handling in the maintained Node middleware.
    const compress = compression({ filter: (request, response) =>
      /^application\/wasm(?:;|$)/.test(String(response.getHeader("Content-Type") ?? "")) || compression.filter(request, response) });
    server.middlewares.use((request, response, next) =>
      compress(request as Parameters<typeof compress>[0], response as Parameters<typeof compress>[1], next));
  },
};
export default defineConfig({ plugins: [zStack(), previewCache], build: { target: "es2022" } });
