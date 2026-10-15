import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { homedir } from "node:os";
import { resolve, join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const { commit } = JSON.parse(readFileSync(join(root, "crates/z-transparent-pir-v2/upstream-sources.json"), "utf8"));
assert.match(commit, /^[0-9a-f]{40}$/);
const cache = join(process.env.CARGO_HOME ?? join(homedir(), ".cargo"), "git/checkouts");
let entries;
try {
  entries = readdirSync(cache);
} catch (error) {
  if (error.code !== "ENOENT") throw error;
  entries = [];
}
const checkouts = entries.filter((entry) => entry.startsWith("wallet-pir-")).map((entry) => join(cache, entry, commit.slice(0, 7)))
  .filter((path) => existsSync(path));
assert.equal(checkouts.length, 1, "Run cargo fetch --locked to obtain the exact pinned transparent dependency before checking its adaptation.");
assert.equal(execFileSync("git", ["rev-parse", "HEAD"], { cwd: checkouts[0], encoding: "utf8" }).trim(), commit);
execFileSync("python3", [join(root, "crates/z-transparent-pir-v2/tools/verify-upstream.py"), checkouts[0]], { cwd: root, stdio: "inherit" });
