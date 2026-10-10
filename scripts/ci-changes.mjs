import { execFileSync } from "node:child_process";
import { appendFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const entrypoints = new Set(["README.md", "CHANGELOG.md", "CONTRIBUTING.md", "SECURITY.md"]);
// test-packages compiles these guides' marked examples against installed exports.
const executableGuides = new Set(["docs/SDK.md", "docs/SERVICES.md"]);
export function docsOnly(paths) {
  return paths.length > 0 && paths.every(path => !executableGuides.has(path) && (entrypoints.has(path)
    || (/^docs\/.+\.md$/.test(path) && !path.startsWith("docs/api/"))
    || /^packages\/(sdk|core|passkey|base)\/README\.md$/.test(path)
    || /^examples\/[^/]+\/(README|HOW-IT-WORKS)\.md$/.test(path)));
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  let full = true;
  if (process.env.CI_EVENT_NAME === "pull_request") {
    const base = process.env.CI_BASE_SHA;
    if (!/^[a-f0-9]{40}$/.test(base ?? "")) throw new Error("Missing PR base revision");
    // Disable rename detection so moving code into a docs path cannot hide its deletion.
    const paths = execFileSync("git", ["diff", "--no-renames", "--name-only", "-z", base, "HEAD"])
      .toString().split("\0").filter(Boolean);
    full = !docsOnly(paths);
  }
  if (!process.env.GITHUB_OUTPUT) throw new Error("Missing workflow output file");
  appendFileSync(process.env.GITHUB_OUTPUT, `full=${full}\n`);
  console.log(full ? "Full engine, SDK and browser verification" : "Documentation-only verification");
}
