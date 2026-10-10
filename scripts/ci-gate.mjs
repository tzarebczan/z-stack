import { pathToFileURL } from "node:url";

export function ciPassed(needs) {
  if (needs.changes?.result !== "success") return false;
  const full = needs.changes.outputs?.full;
  if (full !== "true" && full !== "false") return false;
  const required = full === "true" ? ["native", "sdk", "browser"] : ["docs"];
  const skipped = full === "true" ? ["docs"] : ["native", "sdk", "browser"];
  return required.every(job => needs[job]?.result === "success")
    && skipped.every(job => needs[job]?.result === "skipped");
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const needs = JSON.parse(process.env.CI_NEEDS ?? "null");
  if (!needs || !ciPassed(needs)) {
    console.error("Required verification failed, was cancelled, or was unexpectedly skipped");
    process.exitCode = 1;
  } else console.log("All required CI jobs passed");
}
