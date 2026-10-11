import assert from "node:assert/strict";
import { join, resolve } from "node:path";

const variants = { "single-thread": "wasm-st", "multi-thread": "wasm-mt" };

export function wasmTargetDirectory(
  root,
  mode,
  configuredBase = process.env.CARGO_TARGET_DIR,
) {
  assert.ok(Object.hasOwn(variants, mode), "Unknown WASM build mode");
  return join(resolve(root, configuredBase || "target"), variants[mode]);
}

// Module.imports describes memory imports but does not expose their shared flag.
// Read the validated module's memory types to check its actual thread capability.
function memoryTypes(bytes) {
  let offset = 8;
  const memories = [];
  const byte = () => {
    assert.ok(offset < bytes.length, "Truncated WASM memory metadata");
    return bytes[offset++];
  };
  const integer = () => {
    let value = 0;
    for (let shift = 0; shift < 35; shift += 7) {
      const part = byte();
      value += (part & 127) * 2 ** shift;
      if (!(part & 128)) return value;
    }
    throw new Error("Unsupported WASM memory metadata integer");
  };
  const name = () => {
    const length = integer();
    offset += length;
  };
  const valueType = () => {
    const type = byte();
    if (type === 0x63 || type === 0x64) integer();
  };
  const limits = () => {
    const flags = integer();
    integer();
    if (flags & 1) integer();
    return Boolean(flags & 2);
  };
  while (offset < bytes.length) {
    const section = byte();
    const size = integer();
    const end = offset + size;
    if (section === 2) {
      const count = integer();
      for (let index = 0; index < count; index++) {
        name();
        name();
        switch (byte()) {
          case 0:
            integer();
            break;
          case 1:
            valueType();
            limits();
            break;
          case 2:
            memories.push({ imported: true, shared: limits() });
            break;
          case 3:
            valueType();
            byte();
            break;
          case 4:
            byte();
            integer();
            break;
          default:
            throw new Error("Unsupported WASM import type");
        }
      }
    } else if (section === 5) {
      const count = integer();
      for (let index = 0; index < count; index++)
        memories.push({ imported: false, shared: limits() });
    }
    offset = end;
  }
  return memories;
}

export function assertWasmVariant(bytes, mode) {
  assert.ok(Object.hasOwn(variants, mode), "Unknown WASM build mode");
  const module = new WebAssembly.Module(bytes);
  const threadPool = WebAssembly.Module.exports(module).some(
    (entry) => entry.kind === "function" && entry.name === "initThreadPool",
  );
  const memories = memoryTypes(bytes);
  if (mode === "single-thread") {
    assert.ok(
      !threadPool &&
        !memories.some((memory) => memory.imported || memory.shared),
      "Single-thread WASM must not export initThreadPool or import/shared memory",
    );
  } else {
    assert.ok(
      threadPool && memories.some((memory) => memory.imported && memory.shared),
      "Multi-thread WASM must export initThreadPool and import shared memory",
    );
  }
}
