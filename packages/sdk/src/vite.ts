/**
 * @module @z-stack/sdk/vite
 *
 * Vite plugin for apps that use `@z-stack/sdk`.
 *
 * ```ts
 * // vite.config.ts
 * import { zStack } from "@z-stack/sdk/vite";
 * export default defineConfig({ plugins: [react(), zStack()] });
 * ```
 *
 * It sets what the engine needs: ES-module workers (the scan and prove
 * workers, and the Rayon pool, are split modules), no dependency pre-bundling
 * for the SDK (pre-bundling breaks `new URL(…, import.meta.url)` for the WASM
 * and worker files), file-serving access to the SDK when it is linked from
 * outside the app, and cross-origin isolation headers in dev and preview so
 * the multi-threaded engine can use SharedArrayBuffer.
 *
 * Production hosting needs the same two headers on your HTML (see the README).
 */

// Node built-ins are loaded at config time and typed locally, so the SDK has no
// @types/node or vite dependency.
type NodeApis = {
  realpathSync(path: string): string;
  dirname(path: string): string;
  resolve(...parts: string[]): string;
  fileURLToPath(url: string): string;
};

async function nodeApis(): Promise<NodeApis> {
  const load = (id: string) => import(id as string) as Promise<Record<string, unknown>>;
  const [fs, path, url] = await Promise.all([load("node:fs"), load("node:path"), load("node:url")]);
  return {
    realpathSync: fs.realpathSync as NodeApis["realpathSync"],
    dirname: path.dirname as NodeApis["dirname"],
    resolve: path.resolve as NodeApis["resolve"],
    fileURLToPath: url.fileURLToPath as NodeApis["fileURLToPath"],
  };
}

export type ZStackViteOptions = {
  /** Send COOP/COEP in dev and preview. Default true. Turn off if your config already sets them. */
  crossOriginIsolation?: boolean;
  /**
   * `credentialless` (default) keeps cross-origin images and fonts working;
   * `require-corp` is stricter and needed by some older Safari versions.
   */
  coep?: "credentialless" | "require-corp";
};

type Headers = Record<string, string>;
type Config = {
  worker?: { format?: "es" | "iife" };
  optimizeDeps?: { exclude?: string[] };
  server?: { headers?: Headers };
  preview?: { headers?: Headers };
};
type ResolvedConfig = { server: { fs: { allow: string[] } } };

/** Structurally a Vite `Plugin`; typed locally so the SDK needs no Vite dependency. */
export type ZStackVitePlugin = {
  name: string;
  enforce?: "pre" | "post";
  config: () => Config;
  configResolved: (config: ResolvedConfig) => Promise<void>;
};

export function zStack(options: ZStackViteOptions = {}): ZStackVitePlugin {
  const isolate = options.crossOriginIsolation !== false;
  const headers: Headers = {
    "Cross-Origin-Opener-Policy": "same-origin",
    "Cross-Origin-Embedder-Policy": options.coep ?? "credentialless",
  };
  return {
    name: "z-stack",
    enforce: "pre",
    config: () => ({
      worker: { format: "es" },
      optimizeDeps: { exclude: ["@z-stack/sdk"] },
      ...(isolate ? { server: { headers }, preview: { headers } } : {}),
    }),
    // A linked checkout lives outside the app, so Vite must be allowed to serve
    // it. Added to the resolved list (not `config`), which keeps Vite's default
    // workspace root; setting `server.fs.allow` in `config` would replace it.
    async configResolved(config) {
      const { realpathSync, dirname, resolve, fileURLToPath } = await nodeApis();
      // `dist/vite.js` -> the directory holding @z-stack/{sdk,core,passkey}.
      const packages = realpathSync(resolve(dirname(fileURLToPath(import.meta.url)), "..", ".."));
      if (!config.server.fs.allow.includes(packages)) config.server.fs.allow.push(packages);
    },
  };
}

export default zStack;
