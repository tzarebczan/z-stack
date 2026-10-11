import assert from "node:assert/strict";
import test from "node:test";
import { join } from "node:path";
import {
  assertWasmVariant,
  wasmTargetDirectory,
} from "./wasm-build-variant.mjs";

function integer(value) {
  const bytes = [];
  do {
    const part = value & 127;
    value = Math.floor(value / 128);
    bytes.push(part | (value ? 128 : 0));
  } while (value);
  return bytes;
}
const name = (value) => [...integer(value.length), ...Buffer.from(value)];
const section = (id, bytes) => [id, ...integer(bytes.length), ...bytes];
function moduleBytes({
  imported = false,
  shared = false,
  threadPool = false,
} = {}) {
  const sections = [section(1, [1, 0x60, 0, 0])];
  const memory = [shared ? 3 : 1, 1, 1];
  if (imported)
    sections.push(
      section(2, [1, ...name("env"), ...name("memory"), 2, ...memory]),
    );
  if (threadPool) sections.push(section(3, [1, 0]));
  if (!imported) sections.push(section(5, [1, ...memory]));
  if (threadPool) {
    sections.push(section(7, [1, ...name("initThreadPool"), 0, 0]));
    sections.push(section(10, [1, 2, 0, 0x0b]));
  }
  return new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0, ...sections.flat()]);
}

test("ST and MT always own distinct target children beneath default or configured cache bases", () => {
  const root = join(process.cwd(), "test-project");
  for (const [base, expected] of [
    ["", join(root, "target")],
    ["cache", join(root, "cache")],
    [join(process.cwd(), "shared-cache"), join(process.cwd(), "shared-cache")],
    ["target/wasm-mt", join(root, "target/wasm-mt")],
  ]) {
    const st = wasmTargetDirectory(root, "single-thread", base);
    const mt = wasmTargetDirectory(root, "multi-thread", base);
    assert.equal(st, join(expected, "wasm-st"));
    assert.equal(mt, join(expected, "wasm-mt"));
    assert.notEqual(st, mt);
  }
  assert.throws(() => wasmTargetDirectory(root, "unknown"), /Unknown/);
});

test("actual WASM exports and shared-memory types determine the variant", () => {
  const st = moduleBytes();
  const mt = moduleBytes({ imported: true, shared: true, threadPool: true });
  assert.doesNotThrow(() => assertWasmVariant(st, "single-thread"));
  assert.doesNotThrow(() => assertWasmVariant(mt, "multi-thread"));
  assert.throws(() => assertWasmVariant(st, "multi-thread"), /Multi-thread/);
  assert.throws(() => assertWasmVariant(mt, "single-thread"), /Single-thread/);
  // Importing a nonshared memory alone cannot establish a threaded build.
  assert.throws(
    () =>
      assertWasmVariant(
        moduleBytes({ imported: true, threadPool: true }),
        "multi-thread",
      ),
    /shared memory/,
  );
  assert.throws(
    () => assertWasmVariant(moduleBytes({ shared: true }), "single-thread"),
    /Single-thread/,
  );
  assert.throws(
    () => assertWasmVariant(moduleBytes({ threadPool: true }), "single-thread"),
    /Single-thread/,
  );
  assert.throws(
    () =>
      assertWasmVariant(
        moduleBytes({ imported: true, shared: true }),
        "multi-thread",
      ),
    /initThreadPool/,
  );
  assert.throws(
    () => assertWasmVariant(new Uint8Array([0]), "single-thread"),
    WebAssembly.CompileError,
  );
});
