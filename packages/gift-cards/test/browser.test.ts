import assert from "node:assert/strict";
import { test } from "node:test";
import { createBrowserGiftBackend } from "../src/browser";

// Protocol-only worker fixture. Funded browser qualification exercises real engines separately.
test("Close waits for a committed journal ACK and never turns success into rejection", async t => {
  const original = Object.getOwnPropertyDescriptor(globalThis, "Worker");
  const location = Object.getOwnPropertyDescriptor(globalThis, "location");
  let entered!: () => void, release!: () => void;
  const waiting = new Promise<void>(resolve => { entered = resolve; });
  const commit = new Promise<void>(resolve => { release = resolve; });
  const sent: { op: string; accepted?: boolean }[] = [];
  let terminated = false;
  class WorkerFixture {
    onmessage?: (event: { data: unknown }) => void;
    onerror?: () => void;
    onmessageerror?: () => void;
    send(data: unknown) { queueMicrotask(() => this.onmessage?.({ data })); }
    postMessage(request: { id: number; op: string; accepted?: boolean }) {
      sent.push(request);
      if (request.op === "open") this.send({ id: request.id, kind: "snapshot", value: {} });
      if (request.op === "send") this.send({ id: request.id, kind: "reserved", txid: "aa".repeat(32) });
      if (request.op === "reserved-ack") this.send({ id: request.id, kind: "error", code: "broadcast_failed", txid: "aa".repeat(32) });
      if (request.op === "close") this.send({ id: request.id, kind: "done" });
    }
    terminate() { terminated = true; }
  }
  Object.defineProperty(globalThis, "Worker", { configurable: true, value: WorkerFixture });
  Object.defineProperty(globalThis, "location", { configurable: true, value: { href: "https://wallet.example/" } });
  t.after(() => {
    if (original) Object.defineProperty(globalThis, "Worker", original); else Reflect.deleteProperty(globalThis, "Worker");
    if (location) Object.defineProperty(globalThis, "location", location); else Reflect.deleteProperty(globalThis, "location");
  });
  const backend = createBrowserGiftBackend({ namespace: "fixture", network: "regtest", server: "http://127.0.0.1:1" });
  const wallet = await backend.open({ network: "regtest", entropy: new Uint8Array(32), birthdayHeight: 1, amountZat: 50_000n });
  const sending = wallet.send("recipient", "0.0005", { maxFeeZat: "10000", beforeSubmit: async () => { entered(); await commit; } });
  const rejected = assert.rejects(sending, (error: unknown) => error instanceof Error && "code" in error && error.code === "unresolved");
  await waiting;
  const closing = wallet.close();
  await Promise.resolve();
  assert.equal(terminated, false);
  release();
  await rejected;
  await closing;
  assert.equal(sent.find(message => message.op === "reserved-ack")?.accepted, true);
  assert.equal(terminated, true);
});
