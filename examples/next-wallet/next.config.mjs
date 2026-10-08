import { dirname } from "node:path";
import { fileURLToPath } from "node:url";
// Next's webpack compiler understands the SDK's module-worker URLs. Emit WASM
// as a file: the SDK fetches, verifies and instantiates it itself.
const staticExport = process.env.Z_STACK_STATIC_EXPORT === "1";
const isolated = process.env.Z_STACK_ISOLATION !== "off";
export default {
  outputFileTracingRoot: dirname(fileURLToPath(import.meta.url)),
  poweredByHeader: false,
  agentRules: false,
  ...(staticExport ? { output: "export" } : {}),
  reactStrictMode: true,
  async headers() {
    if (staticExport) return []; // A static host supplies its own headers.
    return [{ source: "/:path*", headers: [
      { key: "Referrer-Policy", value: "no-referrer" },
      { key: "X-Content-Type-Options", value: "nosniff" },
      { key: "X-Frame-Options", value: "DENY" },
      ...(isolated ? [
        { key: "Cross-Origin-Opener-Policy", value: "same-origin" },
        { key: "Cross-Origin-Embedder-Policy", value: "require-corp" },
      ] : []),
    ] }];
  },
  webpack(config) {
    config.module.rules.push({ test: /\.wasm$/, type: "asset/resource" });
    return config;
  },
};
