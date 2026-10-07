import assert from "node:assert/strict";
import { test } from "node:test";
import { walletLifetime } from "../examples/react-wallet/src/lifetime.ts";
import { recoveryMemory } from "../examples/next-wallet/lib/recovery-memory.ts";

test("recovery preparation waits for matching confirmation and clears words before commit", async () => {
  const memory = recoveryMemory(), abort = new AbortController();
  let confirmed = false;
  const ready = memory.prepare("fixture-address", "public-fixture-only", abort.signal).then(() => { confirmed = true; });
  assert.deepEqual(memory.snapshot(), { address: "fixture-address", phrase: "public-fixture-only" });
  memory.acknowledge("different-wallet"); await Promise.resolve();
  assert.equal(confirmed, false);
  memory.acknowledge("fixture-address"); await ready;
  assert.equal(confirmed, true); assert.equal(memory.snapshot(), undefined);
});

test("teardown cancels preparation and a stale acknowledgment cannot confirm a new wallet", async () => {
  const memory = recoveryMemory(), first = new AbortController(), second = new AbortController();
  memory.subscribe(() => { throw new Error("failed render"); });
  const cancelled = assert.rejects(memory.prepare("first", "public-first-phrase", first.signal), { name: "AbortError" });
  first.abort(); await cancelled;
  assert.equal(memory.snapshot(), undefined);
  const next = memory.prepare("second", "public-second-phrase", second.signal);
  memory.acknowledge("first");
  assert.deepEqual(memory.snapshot(), { address: "second", phrase: "public-second-phrase" });
  memory.acknowledge("second"); await next;
  assert.equal(memory.snapshot(), undefined);
});

test("React remount waits for initialization and asynchronous owner teardown", async () => {
  let opened = 0;
  let resolveOpen;
  let resolveClose;
  const opening = new Promise(resolve => { resolveOpen = resolve; });
  const closing = new Promise(resolve => { resolveClose = resolve; });
  const acquire = walletLifetime(async () => { opened++; return opened === 1 ? opening : { close: async () => {} }; });
  const first = acquire();
  await Promise.resolve();
  first.release();
  const second = acquire();
  resolveOpen({ close: () => closing });
  await first.ready;
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(opened, 1);
  resolveClose();
  await second.ready;
  assert.equal(opened, 2);
  second.release();
});

test("StrictMode's discarded setup starts no owner; remount can reopen after a close error", async () => {
  let opened = 0;
  const failure = new Error("close failed");
  const acquire = walletLifetime(async () => { opened++; return { close: async () => { if (opened === 1) throw failure; } }; });
  const discarded = acquire();
  discarded.release();
  assert.equal(await discarded.ready, undefined);
  const mounted = acquire();
  await mounted.ready;
  mounted.release();
  const next = acquire();
  assert.ok(await next.ready);
  next.release();
  assert.equal(opened, 2);
});

test("remount delegates an unreleased owner's busy refusal to the SDK", async () => {
  let active = false;
  const busy = new Error("owner still busy");
  const acquire = walletLifetime(async () => {
    if (active) throw busy;
    active = true;
    return { close: async () => { throw new Error("cleanup could not finish"); } };
  });
  const first = acquire();
  await first.ready;
  first.release();
  const next = acquire();
  await assert.rejects(next.ready, error => error === busy);
  next.release();
});
