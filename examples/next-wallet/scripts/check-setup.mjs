import { readFileSync } from "node:fs";
const manifest = JSON.parse(readFileSync(new URL("../package.json", import.meta.url)));
if (!manifest.dependencies?.["@z-stack/sdk"]) {
  console.error("This is a source template without an installed SDK. Download and scaffold the prebuilt preview: https://github.com/tzarebczan/z-stack/blob/main/docs/GETTING-STARTED.md . Run the helper inside that verified bundle; a fresh source clone has no built archives. For manual setup, install the downloaded SDK archive first; see README.md.");
  process.exit(1);
}
