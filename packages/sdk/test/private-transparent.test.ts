import { test } from "node:test";
import assert from "node:assert/strict";
import { syncPrivateTransparent, type PrivateTransparentHost, type PrivateTransparentOperation } from "../src/private-transparent";
const sourceId = "11".repeat(32);
const hash = "22".repeat(32);
const checkpointId = "33".repeat(32);
const bytes = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
function setup(steps: unknown[]) {
  const calls: string[] = [];
  const abort = new AbortController();
  const source: PrivateTransparentHost = { privateTransparent: async (operation, input) => {
    calls.push(operation);
    switch (operation) {
      case "begin": assert.deepEqual(JSON.parse(new TextDecoder().decode(input)), { source_id: sourceId }); return bytes({ height: 99, hash });
      case "advance": return bytes(steps.shift());
      case "status": return bytes({ completion: "complete-for-enrolled-scope" });
      case "ack": assert.equal(new TextDecoder().decode(input), checkpointId); return new Uint8Array();
      default: return new Uint8Array();
    }
  } };
  return { calls, abort, source, input: {
    source, signal: abort.signal, assertCurrent: () => {},
    checkpoint: async () => { calls.push("persist"); },
    transport: { sourceId, exchange: async (input: { operation: Uint8Array; anchor: { height: number; hash: string } }) => {
      calls.push("exchange"); assert.deepEqual(input.anchor, { height: 99, hash });
      assert.deepEqual(JSON.parse(new TextDecoder().decode(input.operation)), { kind: "map" });
      return bytes({ kind: "data", body: "e30=" });
    } }, treeState: undefined,
  } };
}
test("persists source before network and acknowledges durable checkpoint before completion", async () => {
  const f = setup([
    { kind: "exchange", request_id: 1, operation: { kind: "map" } },
    { kind: "checkpoint", checkpoint_id: checkpointId }, { kind: "stopped", status: { kind: "complete" } },
  ]);
  assert.equal(await syncPrivateTransparent(f.input), "complete");
  assert.deepEqual(f.calls, ["begin", "persist", "advance", "exchange", "accept", "advance", "persist", "ack", "persist", "advance", "status", "cancel"]);
});
test("failed durable storage never acknowledges or starts another exchange", async () => {
  const f = setup([{ kind: "checkpoint", checkpoint_id: checkpointId }]);
  let commits = 0;
  await assert.rejects(syncPrivateTransparent({ ...f.input, checkpoint: async () => { if (++commits === 2) throw new Error("quota"); } }), /quota/);
  assert.deepEqual(f.calls, ["begin", "advance", "cancel"]);
});
test("a late response after wallet replacement is discarded and consumes the old context", async () => {
  const f = setup([{ kind: "exchange", request_id: 1, operation: { kind: "map" } }]);
  let changed = false;
  await assert.rejects(syncPrivateTransparent({ ...f.input,
    assertCurrent: () => { if (changed) throw new Error("wallet_changed"); },
    transport: { sourceId, exchange: async () => { changed = true; return bytes({ kind: "data", body: "e30=" }); } },
  }), /wallet_changed/);
  assert.equal(f.calls.includes("accept"), false);
  assert.equal(f.calls.at(-1), "cancel");
});
test("independent historical chain evidence is passed to Rust without echoing publisher claims", async () => {
  const f = setup([{ kind: "need_chain", height: 10, hash: "44".repeat(32) }, { kind: "stopped", status: { kind: "incomplete", reason: "chain" } }]);
  const prior = f.source.privateTransparent!;
  let evidence: unknown;
  f.source.privateTransparent = async (op: PrivateTransparentOperation, input: Uint8Array) => {
    if (op === "chain") evidence = JSON.parse(new TextDecoder().decode(input));
    return prior(op, input);
  };
  assert.equal(await syncPrivateTransparent({ ...f.input, treeState: async (height) => ({ height, hash }) }), "scanning");
  assert.deepEqual(evidence, { height: 10, hash });
});
test("missing historical source cannot fall back to address or transaction lookups", async () => {
  const f = setup([{ kind: "need_chain", height: 10, hash }]);
  await assert.rejects(syncPrivateTransparent(f.input), /chain_unavailable/);
  assert.deepEqual(f.calls, ["begin", "persist", "advance", "cancel"]);
});
test("cancellation stops a provider that ignores abort without accepting its late response", async () => {
  const f = setup([{ kind: "exchange", request_id: 1, operation: { kind: "map" } }]);
  await assert.rejects(syncPrivateTransparent({ ...f.input, transport: { sourceId, exchange: async () => {
    queueMicrotask(() => f.abort.abort(new Error("cancelled")));
    return new Promise<Uint8Array>(() => {});
  } } }), /cancelled/);
  assert.equal(f.calls.includes("accept"), false);
  assert.equal(f.calls.at(-1), "cancel");
});
test("malformed checkpoint IDs and unsafe operation IDs cannot be acknowledged", async () => {
  for (const step of [{ kind: "checkpoint", checkpoint_id: "not-a-digest" }, { kind: "exchange", request_id: Number.MAX_SAFE_INTEGER + 1, operation: { kind: "map" } }]) {
    const f = setup([step]);
    await assert.rejects(syncPrivateTransparent(f.input), /engine_invalid/);
    assert.equal(f.calls.includes("ack"), false);
    assert.equal(f.calls.includes("exchange"), false);
  }
});
test("confirmed activity repeats discovery for the expanded wallet scope before completion", async () => {
  const f = setup([
    { kind: "stopped", status: { kind: "complete" } },
    { kind: "stopped", status: { kind: "complete" } },
  ]);
  const original = f.source.privateTransparent!;
  let scopes = 0;
  f.source.privateTransparent = async (op, input) => {
    if (op === "status") return bytes({ completion: ++scopes === 1 ? "incomplete" : "complete-for-enrolled-scope" });
    return original(op, input);
  };
  assert.equal(await syncPrivateTransparent(f.input), "complete");
  assert.equal(scopes, 2);
  assert.equal(f.calls.filter((op) => op === "begin").length, 2);
});
test("an undurable acknowledged state cannot be published as complete", async () => {
  const f = setup([{ kind: "checkpoint", checkpoint_id: checkpointId }, { kind: "stopped", status: { kind: "complete" } }]);
  let writes = 0;
  await assert.rejects(syncPrivateTransparent({ ...f.input, checkpoint: async () => {
    f.calls.push("persist"); if (++writes === 3) throw new Error("cross_tab_conflict");
  } }), /cross_tab_conflict/);
  assert.deepEqual(f.calls, ["begin", "persist", "advance", "persist", "ack", "persist", "cancel"]);
});
