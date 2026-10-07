import assert from "node:assert/strict";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { nativeDiagnosticViolations, privateDiagnosticViolations } from "../../../scripts/check-private-diagnostics.mjs";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createEventBus } from "../src/events.ts";

test("wallet diagnostics cannot forward data or raw error callbacks to the console", () => {
  const roots = ["sdk", "core", "passkey"].map((name) => fileURLToPath(new URL(`../../${name}/src/`, import.meta.url)));
  // vite.ts is build-tool output, not runtime wallet diagnostics.
  assert.deepEqual(privateDiagnosticViolations(roots, new Set(["vite.ts"])), []);
});

test("engine diagnostics reject structured fields, raw errors and interpolated identifiers", () => {
  assert.deepEqual(nativeDiagnosticViolations(new URL("../../../crates/z-engine/src/", import.meta.url)), []);
  const fixture = mkdtempSync(join(tmpdir(), "z-stack-native-diagnostics-"));
  try {
    writeFileSync(join(fixture, "example.rs"), 'info!("sync complete");\nwarn!(error = %e, "sync failed");\ntracing::debug!("tx {txid}");\ninfo!("sync {}", height);');
    assert.equal(nativeDiagnosticViolations(fixture).length, 3);
  } finally { rmSync(fixture, { recursive: true, force: true }); }
});

test("a failing consumer callback does not disclose its exception or block other listeners", () => {
  const bus = createEventBus();
  const captured: unknown[][] = [];
  const previous = console.warn;
  const privatePayload = new Error("private mnemonic, address, token and memo sentinel");
  let inspected = false;
  Object.defineProperty(privatePayload, "message", { get() { inspected = true; throw new Error("do not inspect wallet data"); } });
  let delivered = false;
  bus.on("sync", () => { throw privatePayload; });
  bus.on("sync", () => { delivered = true; });
  console.warn = (...args) => { captured.push(args); };
  try {
    bus.emit("sync", { stage: "synced", scanned: 123 });
    assert.deepEqual(captured, [["wallet event handler"]]);
    assert.equal(inspected, false);
    assert.equal(delivered, true);
  } finally { console.warn = previous; }
});
