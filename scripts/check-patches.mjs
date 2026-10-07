#!/usr/bin/env node
// Offline tree/patch verification. --archives also verifies cached .crate bytes;
// neither mode downloads archives or independently authenticates the manifest.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { cpSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { gunzipSync } from "node:zlib";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const sha256 = bytes => createHash("sha256").update(bytes).digest("hex");
const validDigest = value => typeof value === "string" && /^[a-f0-9]{64}$/.test(value);
const safePath = value => typeof value === "string" && value.length > 0 &&
  !value.includes("\\") && !value.startsWith("/") && value.split("/").every(part => part && part !== "." && part !== "..");

function treeHashes(dir) {
  const walk = directory => readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const path = join(directory, entry.name);
    assert.ok(entry.isDirectory() || entry.isFile(), `unsupported vendored entry: ${path}`);
    return entry.isDirectory() ? walk(path) : [path];
  });
  return Object.fromEntries(walk(dir).map(path => [relative(dir, path).split("\\").join("/"), sha256(readFileSync(path))]).sort());
}

function sameFiles(actual, expected, label) {
  assert.deepEqual(Object.keys(actual).sort(), Object.keys(expected).sort(), `${label}: files differ`);
  for (const [path, hash] of Object.entries(expected)) assert.equal(actual[path], hash, `${label}/${path}: bytes differ`);
}

/** Read regular-file Cargo tar entries in memory; never extract archive paths. */
function archiveHashes(bytes, prefix) {
  const tar = gunzipSync(bytes, { maxOutputLength: 32 * 1024 * 1024 });
  const hashes = {};
  const text = field => field.toString("utf8").replace(/\0.*$/s, "");
  const octal = field => {
    const value = text(field).trim();
    assert.match(value, /^[0-7]+$/, "unsupported tar numeric field");
    return Number.parseInt(value, 8);
  };
  let offset = 0;
  for (; offset + 512 <= tar.length; ) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every(byte => byte === 0)) break;
    const checksum = header.reduce((sum, byte, index) => sum + (index >= 148 && index < 156 ? 32 : byte), 0);
    assert.equal(checksum, octal(header.subarray(148, 156)), "invalid tar header checksum");
    const namePrefix = text(header.subarray(345, 500));
    const name = (namePrefix ? `${namePrefix}/` : "") + text(header.subarray(0, 100));
    const type = header[156];
    assert.ok(type === 0 || type === 48 || type === 53, "archive contains a non-regular entry");
    assert.ok(name.startsWith(prefix), "archive entry has the wrong crate prefix");
    const path = name.slice(prefix.length).replace(/\/$/, "");
    assert.ok(type === 53 && path === "" || safePath(path), "unsafe archive path");
    const size = octal(header.subarray(124, 136));
    const end = offset + 512 + size;
    assert.ok(end <= tar.length, "truncated tar entry");
    if (type !== 53) {
      assert.ok(!Object.hasOwn(hashes, path), "duplicate archive file");
      Object.defineProperty(hashes, path, { value: sha256(tar.subarray(offset + 512, end)), enumerable: true });
    }
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  assert.ok(tar.length - offset >= 1024 && tar.subarray(offset).every(byte => byte === 0), "missing or invalid tar terminator");
  return hashes;
}

export function verifyPatches(sourceRoot, archivesDirectory) {
  const patches = join(sourceRoot, "patches", "zakura");
  const manifest = JSON.parse(readFileSync(join(patches, "manifest.json"), "utf8"));
  const scratch = mkdtempSync(join(tmpdir(), "z-stack-patches-"));
  const results = [];
  try {
    for (const crate of manifest.crates) {
      assert.match(crate.package, /^[a-z][a-z0-9_-]*$/, "invalid crate package");
      assert.match(crate.version, /^\d+\.\d+\.\d+(?:[-+][a-zA-Z0-9.-]+)?$/, "invalid crate version");
      assert.equal(crate.patch, `${crate.package}.patch`, "invalid patch filename");
      assert.ok(validDigest(crate.sha256), `${crate.package}: archive digest must be 64 lowercase hex characters`);
      for (const [path, hash] of Object.entries(crate.files)) {
        assert.ok(safePath(path) && validDigest(hash), `${crate.package}: invalid baseline path or digest`);
      }
      const original = treeHashes(join(sourceRoot, "vendor", "zakura", crate.package));
      const copy = join(scratch, crate.package);
      cpSync(join(sourceRoot, "vendor", "zakura", crate.package), copy, { recursive: true });
      const apply = extra => execFileSync("git", ["-c", "apply.ignoreWhitespace=no", "apply", "--whitespace=nowarn", ...extra, join(patches, crate.patch)], { cwd: scratch });
      // Git requires matching context; no whitespace ignoring or three-way fallback.
      apply(["--reverse"]);
      const baseline = treeHashes(copy);
      sameFiles(baseline, crate.files, `${crate.package}: recorded baseline`);
      if (archivesDirectory) {
        const archivePath = join(archivesDirectory, `${crate.package}-${crate.version}.crate`);
        assert.ok(statSync(archivePath).size <= 32 * 1024 * 1024, "cached archive exceeds size limit");
        const archive = readFileSync(archivePath);
        assert.equal(sha256(archive), crate.sha256, `${crate.package}: cached archive digest differs`);
        sameFiles(archiveHashes(archive, `${crate.package}-${crate.version}/`), baseline, `${crate.package}: cached archive baseline`);
      }
      apply([]);
      sameFiles(treeHashes(copy), original, `${crate.package}: forward patch round-trip`);
      results.push({ package: crate.package, version: crate.version, archiveVerified: !!archivesDirectory });
    }
    return results;
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  assert.ok(args.length === 0 || args.length === 2 && args[0] === "--archives", "usage: check-patches.mjs [--archives <cached-crate-directory>]");
  for (const crate of verifyPatches(root, args[1] ? resolve(args[1]) : undefined)) {
    console.log(`${crate.package} ${crate.version}: recorded baseline and exact patch round-trip verified${crate.archiveVerified ? "; cached archive verified" : " (offline; archive not checked)"}`);
  }
}
