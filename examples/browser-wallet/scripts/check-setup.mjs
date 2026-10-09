import { readFileSync } from "node:fs";
const manifest = JSON.parse(readFileSync(new URL("../package.json", import.meta.url)));
if (!manifest.dependencies?.["@z-stack/sdk"]) {
  console.error("This is a source template. Generate an app with the published preview: node scripts/create-example.mjs browser-wallet /path/to/my-wallet --install. For manual setup, install the SDK archive first; see README.md.");
  process.exit(1);
}
