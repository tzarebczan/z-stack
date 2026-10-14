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

test("host resumes supplied durable chain checkpoints and honors core budget yields across cold hosts", async () => {
  // Host contract stubs: the real core resource budget and cache are covered by Rust integration tests.
  for (const budget of [undefined, 32]) {
    let durableThrough = 0;
    const chainReads: number[][] = [];
    const expectedReads = budget === undefined ? [64, 64, 2] : [32, 32, 32, 32, 2];
    for (let coldRun = 0; coldRun < expectedReads.length; coldRun++) {
      let acceptedThrough = durableThrough;
      let staged = false, yielded = false;
      const reads: number[] = [];
      chainReads.push(reads);
      const engine: PrivateTransparentHost = { privateTransparent: async (operation, input) => {
        switch (operation) {
          case "begin": return bytes({ height: 200, hash });
          case "advance":
            if (input.length) {
              assert.deepEqual(JSON.parse(new TextDecoder().decode(input)), { budget_yield: true });
              yielded = true;
            }
            if (staged) return bytes({ kind: "checkpoint", checkpoint_id: checkpointId });
            if (yielded) return bytes({ kind: "stopped", status: { kind: "incomplete", reason: "chain_budget" } });
            return acceptedThrough < 130
              ? bytes({ kind: "need_chain", height: acceptedThrough + 1, hash })
              : bytes({ kind: "stopped", status: { kind: "complete" } });
          case "chain": {
            const point = JSON.parse(new TextDecoder().decode(input));
            assert.deepEqual(point, { height: acceptedThrough + 1, hash });
            acceptedThrough++;
            staged = budget === undefined || reads.length === budget || acceptedThrough === 130;
            return new Uint8Array();
          }
          case "ack":
            assert.equal(durableThrough, acceptedThrough, "chain progress must commit before acknowledgment");
            staged = false;
            yielded = budget !== undefined && reads.length >= budget;
            return new Uint8Array();
          case "status": return bytes({ completion: "complete-for-enrolled-scope" });
          default: return new Uint8Array();
        }
      } };
      const result = await syncPrivateTransparent({
        source: engine, signal: new AbortController().signal, assertCurrent: () => {},
        checkpoint: async () => { durableThrough = acceptedThrough; },
        treeState: async height => { reads.push(height); return { height, hash }; },
        transport: { sourceId, exchange: async () => { throw new Error("unexpected_private_exchange"); } },
      });
      assert.equal(result, coldRun === expectedReads.length - 1 ? "complete" : "scanning");
    }
    assert.ok(chainReads.every(reads => reads.length <= 64), "host chain-read defense budget stays bounded");
    assert.deepEqual(chainReads.map(reads => reads.length), expectedReads);
    assert.deepEqual(chainReads.flat(), Array.from({ length: 130 }, (_, i) => i + 1));
    assert.equal(durableThrough, 130);
  }
});


test("own deadline durably yields validated chain progress before cancelling the live pass", async t => {
  const deadline = new AbortController();
  t.mock.method(AbortSignal, "timeout", () => deadline.signal);
  const f = setup([
    { kind: "need_chain", height: 1, hash }, { kind: "need_chain", height: 2, hash },
    { kind: "checkpoint", checkpoint_id: checkpointId }, { kind: "stopped", status: { kind: "incomplete", reason: "budget" } },
  ]);
  const original = f.source.privateTransparent!;
  let yielded = false;
  f.source.privateTransparent = async (op, input) => {
    if (op === "advance" && input.length) {
      assert.deepEqual(JSON.parse(new TextDecoder().decode(input)), { budget_yield: true });
      yielded = true;
    }
    return original(op, input);
  };
  const result = await syncPrivateTransparent({ ...f.input, treeState: async height => {
    if (height === 1) return { height, hash };
    deadline.abort(new DOMException("owned deadline", "TimeoutError"));
    return new Promise<{ height: number; hash: string }>(() => {});
  } });
  assert.equal(result, "scanning");
  assert.equal(yielded, true);
  assert.deepEqual(f.calls, ["begin", "persist", "advance", "chain", "advance", "advance", "persist", "ack", "persist", "advance", "cancel"]);
});

test("user cancellation and wallet replacement win over the elapsed budget without flushing", async t => {
  for (const replacement of [false, true]) {
    const deadline = new AbortController();
    const mock = t.mock.method(AbortSignal, "timeout", () => deadline.signal);
    const f = setup([{ kind: "need_chain", height: 1, hash }]);
    let changed = false;
    await assert.rejects(syncPrivateTransparent({ ...f.input,
      assertCurrent: () => { if (changed) throw new Error("wallet_changed"); },
      treeState: async () => {
        if (replacement) changed = true;
        else f.abort.abort(new Error("user_cancelled"));
        deadline.abort(new DOMException("owned deadline", "TimeoutError"));
        return new Promise<{ height: number; hash: string }>(() => {});
      },
    }), replacement ? /wallet_changed/ : /user_cancelled/);
    assert.deepEqual(f.calls, ["begin", "persist", "advance", "cancel"]);
    mock.mock.restore();
  }
});

test("coincident deadline does not convert a storage failure into a budget checkpoint", async t => {
  const deadline = new AbortController();
  t.mock.method(AbortSignal, "timeout", () => deadline.signal);
  const f = setup([{ kind: "checkpoint", checkpoint_id: checkpointId }]);
  const quota = new Error("storage_quota");
  let writes = 0;
  await assert.rejects(syncPrivateTransparent({ ...f.input, checkpoint: async () => {
    if (++writes === 2) {
      deadline.abort(new DOMException("owned deadline", "TimeoutError"));
      throw quota;
    }
  } }), error => error === quota);
  assert.deepEqual(f.calls, ["begin", "advance", "cancel"]);
});

test("budget checkpoint storage failure never acknowledges or starts network work", async t => {
  const deadline = new AbortController();
  t.mock.method(AbortSignal, "timeout", () => deadline.signal);
  const f = setup([
    { kind: "need_chain", height: 1, hash }, { kind: "checkpoint", checkpoint_id: checkpointId },
  ]);
  let writes = 0;
  await assert.rejects(syncPrivateTransparent({ ...f.input,
    treeState: async () => {
      deadline.abort(new DOMException("owned deadline", "TimeoutError"));
      return new Promise<{ height: number; hash: string }>(() => {});
    },
    checkpoint: async () => { if (++writes === 2) throw new Error("storage_quota"); },
  }), /storage_quota/);
  assert.equal(writes, 2);
  assert.deepEqual(f.calls, ["begin", "advance", "advance", "cancel"]);
});

test("deadline after a completed checkpoint preserves durable completion", async t => {
  const deadline = new AbortController();
  t.mock.method(AbortSignal, "timeout", () => deadline.signal);
  const f = setup([
    { kind: "checkpoint", checkpoint_id: checkpointId },
    { kind: "checkpoint", checkpoint_id: checkpointId }, { kind: "stopped", status: { kind: "complete" } },
  ]);
  let writes = 0;
  const result = await syncPrivateTransparent({ ...f.input, checkpoint: async () => {
    f.calls.push("persist");
    if (++writes === 2) deadline.abort(new DOMException("owned deadline", "TimeoutError"));
  } });
  assert.equal(result, "complete");
  assert.equal(f.calls.filter(op => op === "ack").length, 1);
  assert.equal(f.calls.includes("exchange"), false);
});

test("coincident deadline preserves an engine protocol error instead of flushing", async t => {
  const deadline = new AbortController();
  t.mock.method(AbortSignal, "timeout", () => deadline.signal);
  const f = setup([]);
  const original = f.source.privateTransparent!;
  const protocol = new Error("transparent_pir_protocol_invalid");
  f.source.privateTransparent = async (op, input) => {
    if (op === "advance") {
      f.calls.push(op);
      deadline.abort(new DOMException("owned deadline", "TimeoutError"));
      throw protocol;
    }
    return original(op, input);
  };
  await assert.rejects(syncPrivateTransparent(f.input), error => error === protocol);
  assert.deepEqual(f.calls, ["begin", "persist", "advance", "cancel"]);
});

test("lock arriving during the budget checkpoint prevents acknowledgment", async t => {
  const deadline = new AbortController();
  t.mock.method(AbortSignal, "timeout", () => deadline.signal);
  const f = setup([
    { kind: "need_chain", height: 1, hash }, { kind: "checkpoint", checkpoint_id: checkpointId },
  ]);
  let writes = 0;
  await assert.rejects(syncPrivateTransparent({ ...f.input,
    treeState: async () => {
      deadline.abort(new DOMException("owned deadline", "TimeoutError"));
      return new Promise<{ height: number; hash: string }>(() => {});
    },
    checkpoint: async () => { if (++writes === 2) f.abort.abort(new Error("wallet_locked")); },
  }), /wallet_locked/);
  assert.deepEqual(f.calls, ["begin", "advance", "advance", "cancel"]);
});

test("elapsed deadline after durable completion does not downgrade or acknowledge twice", async t => {
  const deadline = new AbortController();
  t.mock.method(AbortSignal, "timeout", () => deadline.signal);
  const f = setup([{ kind: "checkpoint", checkpoint_id: checkpointId }]);
  const original = f.source.privateTransparent!;
  f.source.privateTransparent = async (op, input) => {
    if (op === "advance" && input.length) {
      f.calls.push(op);
      return bytes({ kind: "stopped", status: { kind: "complete" } });
    }
    return original(op, input);
  };
  let writes = 0;
  assert.equal(await syncPrivateTransparent({ ...f.input, checkpoint: async () => {
    if (++writes === 3) deadline.abort(new DOMException("owned deadline", "TimeoutError"));
  } }), "complete");
  assert.equal(writes, 3);
  assert.equal(f.calls.filter(op => op === "ack").length, 1);
});

for (const drain of [false, true]) {
  test(`metadata policy limit is a closed terminal result${drain ? " during budget drain" : ""}`, async t => {
    const deadline = new AbortController();
    t.mock.method(AbortSignal, "timeout", () => deadline.signal);
    const f = setup([
      ...(drain ? [{ kind: "need_chain", height: 1, hash }] : []),
      { kind: "checkpoint", checkpoint_id: checkpointId },
      { kind: "stopped", status: { kind: "incomplete", reason: "history_metadata_limit" } },
    ]);
    const result = await syncPrivateTransparent({ ...f.input, treeState: async () => {
      deadline.abort(new DOMException("owned deadline", "TimeoutError"));
      return new Promise<{ height: number; hash: string }>(() => {});
    } });
    assert.equal(result, "history-metadata-limit");
    assert.equal(f.calls.filter(op => op === "ack").length, 1);
    assert.equal(f.calls.includes("exchange"), false);
    assert.equal(f.calls.includes("status"), false);
  });
}
test("only the exact incomplete metadata limit is terminal", async () => {
  for (const status of [{ kind: "incomplete", reason: "history_metadata_limit_extra" }, { kind: "incomplete", reason: "chain_budget" }]) {
    const f = setup([{ kind: "stopped", status }]);
    assert.equal(await syncPrivateTransparent(f.input), "scanning");
  }
});
