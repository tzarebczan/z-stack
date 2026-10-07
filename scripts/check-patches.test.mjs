import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { gzipSync } from "node:zlib";
import { verifyPatches } from "./check-patches.mjs";

const digest = bytes => createHash("sha256").update(bytes).digest("hex");
const original = "// original\npub const VALUE: u8 = 1;\n";
const patched = "// original\npub const VALUE: u8 = 2;\n";

function tarArchive(entries) {
  const blocks = [];
  for (const [name, value, type = "0"] of entries) {
    const bytes = Buffer.from(value);
    const header = Buffer.alloc(512);
    header.write(name, 0, 100);
    header.write("0000644\0", 100);
    header.write("0000000\0", 108);
    header.write("0000000\0", 116);
    header.write(bytes.length.toString(8).padStart(11, "0") + "\0", 124);
    header.write("00000000000\0", 136);
    header.fill(32, 148, 156);
    header.write(type, 156);
    header.write("ustar\0", 257);
    const sum = header.reduce((sum, byte) => sum + byte, 0);
    header.write(sum.toString(8).padStart(6, "0") + "\0 ", 148);
    blocks.push(header, bytes, Buffer.alloc((512 - bytes.length % 512) % 512));
  }
  return gzipSync(Buffer.concat([...blocks, Buffer.alloc(1024)]));
}

function fixture(t) {
  const root = mkdtempSync(join(tmpdir(), "z-stack-patch-test-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const vendor = join(root, "vendor", "zakura", "zakura-test", "src");
  const patches = join(root, "patches", "zakura");
  const cache = join(root, "cache");
  for (const path of [vendor, patches, cache]) mkdirSync(path, { recursive: true });
  writeFileSync(join(vendor, "lib.rs"), patched);
  writeFileSync(join(patches, "zakura-test.patch"), `diff --git a/zakura-test/src/lib.rs b/zakura-test/src/lib.rs
--- a/zakura-test/src/lib.rs
+++ b/zakura-test/src/lib.rs
@@ -1,2 +1,2 @@
 // original
-pub const VALUE: u8 = 1;
+pub const VALUE: u8 = 2;
`);
  const archive = tarArchive([["zakura-test-1.2.0/src/lib.rs", original]]);
  const archivePath = join(cache, "zakura-test-1.2.0.crate");
  writeFileSync(archivePath, archive);
  const manifest = { crates: [{ package: "zakura-test", version: "1.2.0", patch: "zakura-test.patch",
    sha256: digest(archive), files: { "src/lib.rs": digest(original) } }] };
  const save = () => writeFileSync(join(patches, "manifest.json"), JSON.stringify(manifest));
  save();
  return { root, vendor, patches, cache, archivePath, manifest, save };
}

test("offline patch round-trip preserves exact vendor bytes; cached mode checks an independent archive", t => {
  const f = fixture(t);
  assert.equal(verifyPatches(f.root)[0].archiveVerified, false);
  assert.equal(verifyPatches(f.root, f.cache)[0].archiveVerified, true);
  assert.equal(readFileSync(join(f.vendor, "lib.rs"), "utf8"), patched);
});

test("extra source files and invalid archive digests fail the offline gate", t => {
  const f = fixture(t);
  writeFileSync(join(f.vendor, "extra.rs"), "undocumented");
  assert.throws(() => verifyPatches(f.root), /files differ/);
  rmSync(join(f.vendor, "extra.rs"));
  f.manifest.crates[0].sha256 = "not-a-sha256";
  f.save();
  assert.throws(() => verifyPatches(f.root), /archive digest must be/);
});

test("cached archive rejects a valid-looking digest mismatch", t => {
  const f = fixture(t);
  f.manifest.crates[0].sha256 = "0".repeat(64);
  f.save();
  assert.throws(() => verifyPatches(f.root, f.cache), /cached archive digest differs/);
});

test("mutually changed tree, patch and file hashes cannot pass an unchanged cached archive", t => {
  const f = fixture(t);
  const claimed = original.replace("u8 = 1", "u8 = 3");
  writeFileSync(join(f.patches, "zakura-test.patch"), readFileSync(join(f.patches, "zakura-test.patch"), "utf8").replace("-pub const VALUE: u8 = 1", "-pub const VALUE: u8 = 3"));
  f.manifest.crates[0].files["src/lib.rs"] = digest(claimed);
  f.save();
  assert.equal(verifyPatches(f.root)[0].archiveVerified, false, "offline mode has no independent trust anchor");
  assert.throws(() => verifyPatches(f.root, f.cache), /cached archive baseline.*bytes differ/);
});

test("cached tar paths and links are rejected without extracting them", t => {
  const f = fixture(t);
  for (const entry of [["zakura-test-1.2.0/../../escape", original], ["zakura-test-1.2.0/src/lib.rs", "", "2"]]) {
    const archive = tarArchive([entry]);
    writeFileSync(f.archivePath, archive);
    f.manifest.crates[0].sha256 = digest(archive);
    f.save();
    assert.throws(() => verifyPatches(f.root, f.cache), /unsafe archive path|non-regular entry/);
  }
});
