import assert from "node:assert/strict";
import { test } from "node:test";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { REGTEST_FAUCET_MNEMONIC, deriveAccount, initialize, seedFingerprint } from "../src/lab.ts";
import { hardwareCancelled, keystoneSigner, ledgerAccount, ledgerSigner, type LedgerTransport } from "../src/hardware.ts";
import { isWalletError } from "@z-stack/core";

const gen = join(dirname(fileURLToPath(import.meta.url)), "..", "src", "generated", "z_wasm_bg.wasm");
const OK = [0x90, 0x00];

async function ready(t: { skip: (m: string) => void }): Promise<boolean> {
  if (!existsSync(gen)) {
    t.skip("run pnpm build:wasm first");
    return false;
  }
  await initialize({ wasmModule: readFileSync(gen), prewarmProvingKey: false, prewarmProveWorker: false });
  return true;
}

/** A Ledger that answers app info and the UFVK export, and records everything. */
function fakeLedger(opts: { app?: string; version?: string; ufvk: string; script?: (apdu: Uint8Array) => number[] | undefined }) {
  const sent: Uint8Array[] = [];
  const ufvk = new TextEncoder().encode(opts.ufvk);
  const payload = [ufvk.length >> 8, ufvk.length & 0xff, ...ufvk];
  let offset = 0;
  const transport: LedgerTransport = {
    async exchange(apdu) {
      sent.push(apdu);
      const scripted = opts.script?.(apdu);
      if (scripted) return new Uint8Array(scripted);
      const [cla, ins, p1] = apdu;
      if (cla === 0xb0 && ins === 0x01) {
        const name = new TextEncoder().encode(opts.app ?? "Zcash");
        const version = new TextEncoder().encode(opts.version ?? "3.9.4");
        return new Uint8Array([1, name.length, ...name, version.length, ...version, 1, 0, ...OK]);
      }
      if (cla === 0xe0 && ins === 0x50) {
        if (p1 === 0x00) offset = 0;
        const chunk = payload.slice(offset, offset + 250);
        offset += chunk.length;
        return new Uint8Array([...chunk, ...OK]);
      }
      return new Uint8Array(OK);
    },
  };
  return { transport, sent };
}

/** Only the exclusive Web Locks behavior used by spends; Node 22 has no LockManager. */
function spendLocks() {
  type Granted<T> = (lock: { name: string; mode: "exclusive" } | null) => Promise<T>;
  const held = new Set<string>();
  return {
    async request<T>(name: string, options: { ifAvailable: true } | Granted<T>, callback?: Granted<T>): Promise<T> {
      assert.equal(name, "z-stack-wallet-spend");
      const run = typeof options === "function" ? options : callback!;
      if (held.has(name)) {
        assert.equal(typeof options === "object" && options.ifAvailable, true, "a spend must not queue behind another tab");
        return run(null);
      }
      held.add(name);
      try {
        return await run({ name, mode: "exclusive" });
      } finally {
        held.delete(name);
      }
    },
  };
}

test("ledgerAccount reads the viewing key the device approves", async (t) => {
  if (!(await ready(t))) return;
  const ufvk = deriveAccount(REGTEST_FAUCET_MNEMONIC, "mainnet", 0).ufvk;
  const { transport, sent } = fakeLedger({ ufvk });
  const account = await ledgerAccount(transport, { network: "mainnet" });
  assert.equal(account.device, "ledger");
  assert.equal(account.ufvk, ufvk);
  assert.equal(account.accountIndex, 0);
  assert.match(account.seedFingerprint, /^[0-9a-f]{64}$/);
  // App info, the first GET_VK, then continuations until the declared length.
  assert.deepEqual([...sent[0]!.slice(0, 2)], [0xb0, 0x01]);
  assert.ok(sent.filter((a) => a[1] === 0x50).length >= 2, "long keys arrive in several chunks");
  // Same key, same stand-in fingerprint: a returning Ledger is recognised.
  assert.equal((await ledgerAccount(fakeLedger({ ufvk }).transport, { network: "mainnet" })).seedFingerprint, account.seedFingerprint);
});

test("ledgerAccount refuses the dashboard, old apps and test networks", async (t) => {
  if (!(await ready(t))) return;
  const ufvk = deriveAccount(REGTEST_FAUCET_MNEMONIC, "mainnet", 0).ufvk;
  const code = async (p: Promise<unknown>) => {
    try {
      await p;
    } catch (e) {
      assert.ok(isWalletError(e), String(e));
      return e.code;
    }
    assert.fail("expected a rejection");
  };
  assert.equal(await code(ledgerAccount(fakeLedger({ ufvk, app: "BOLOS", version: "1.4.0" }).transport, { network: "mainnet" })), "hardware_app");
  assert.equal(await code(ledgerAccount(fakeLedger({ ufvk, version: "3.9.3" }).transport, { network: "mainnet" })), "hardware_app");
  assert.equal(await code(ledgerAccount(fakeLedger({ ufvk }).transport, { network: "testnet" })), "hardware_unsupported");
  const locked = fakeLedger({ ufvk, script: (a) => (a[0] === 0xb0 ? [0x55, 0x15] : undefined) });
  assert.equal(await code(ledgerAccount(locked.transport, { network: "mainnet" })), "hardware_locked");
});

test("ledgerSigner replays a busy review, stops at a rejection, then cools down", async (t) => {
  if (!(await ready(t))) return;
  let busy = 1;
  const { transport, sent } = fakeLedger({
    ufvk: "unused",
    script: (a) => {
      if (a[1] === 0x56 && busy-- > 0) return [0x69, 0x01];
      if (a[1] === 0x57) return [0x69, 0x85];
      return undefined;
    },
  });
  const signer = ledgerSigner(transport);
  assert.equal(await signer.appVersion(), "3.9.4");
  const plan = {
    commands: [
      { cla: 0xe0, ins: 0x52, p1: 0, p2: 0, data: "00" },
      { cla: 0xe0, ins: 0x56, p1: 0, p2: 1, data: "00" },
      { cla: 0xe0, ins: 0x57, p1: 0, p2: 0, data: "" },
      { cla: 0xe0, ins: 0x57, p1: 0, p2: 1, data: "" },
    ],
    reviewIndex: 1,
    signatures: [{ pool: "orchard" as const, actionIndex: 0 }, { pool: "orchard" as const, actionIndex: 1 }],
  };
  const stages: string[] = [];
  const responses = await signer.exchange(plan, (s) => stages.push(s));
  assert.deepEqual(stages, ["reviewing", "signing"]);
  // Busy once (replayed), then the rejection ends the exchange.
  assert.equal(sent.filter((a) => a[1] === 0x56).length, 2);
  assert.equal(responses.length, 3);
  assert.deepEqual([...responses[2]!], [0x69, 0x85]);
  // APDU framing: CLA INS P1 P2 Lc data.
  assert.deepEqual([...sent.find((a) => a[1] === 0x52)!], [0xe0, 0x52, 0, 0, 1, 0]);
  const start = Date.now();
  await signer.appVersion();
  assert.ok(Date.now() - start >= 3_500, "the app gets its post-signing pause");
});

test("hardware wallets restore, refuse seed sends, and ask the device nothing they cannot send", async (t) => {
  if (!(await ready(t))) return;
  const navigatorBefore = Object.getOwnPropertyDescriptor(globalThis, "navigator");
  t.after(() => {
    if (navigatorBefore) Object.defineProperty(globalThis, "navigator", navigatorBefore);
    else Reflect.deleteProperty(globalThis, "navigator");
  });
  // Exercise the production fallback independently of the host Node version.
  Object.defineProperty(globalThis, "navigator", { configurable: true, value: {} });
  const { createWasmClient } = await import("../src/wasm-client.ts");
  let tip = async () => 2;
  const client = createWasmClient({
    network: "regtest",
    transport: { kind: "mock", label: "mock", tip: () => tip(), blocks: async () => new Uint8Array(), submit: async () => "" },
  });
  const ufvk = deriveAccount(REGTEST_FAUCET_MNEMONIC, "regtest", 0).ufvk;
  const account = { device: "keystone" as const, ufvk, seedFingerprint: seedFingerprint(REGTEST_FAUCET_MNEMONIC), accountIndex: 0 };
  const w = await client.restoreHardware(account, "regtest", 1);
  assert.equal(w.viewOnly, false, "a hardware wallet can spend");
  assert.deepEqual(w.hardware, { device: "keystone", seedFingerprint: account.seedFingerprint, accountIndex: 0 });
  assert.equal(client.hasSpendingSeed(), false);
  assert.deepEqual((await client.getWallet()).hardware, w.hardware);
  await assert.rejects(() => client.send(w.unifiedAddress, "0.001"), /pass \{ signer \}/);
  // Its seed turning up (a passkey or passphrase vault) must not bypass the device.
  await assert.rejects(() => client.attachSeed(REGTEST_FAUCET_MNEMONIC), /hardware-wallet account/);
  assert.equal(client.hasSpendingSeed(), false);
  // Unsynced and unfunded: refused before the device is asked anything.
  let asked = false;
  const signer = keystoneSigner(async () => {
    asked = true;
    throw hardwareCancelled();
  });
  await assert.rejects(() => client.send(w.unifiedAddress, "0.001", undefined, { signer }), /sync|insufficient/i);
  assert.equal(asked, false);

  // Without Web Locks, an in-flight spend still excludes a second local spend.
  let releaseTip!: () => void;
  const tipHeld = new Promise<void>((r) => (releaseTip = r));
  let tipStarted!: () => void;
  const readingTip = new Promise<void>((r) => (tipStarted = r));
  tip = async () => {
    tipStarted();
    await tipHeld;
    return 2;
  };
  const first = assert.rejects(() => client.send(w.unifiedAddress, "0.001", undefined, { signer }), /sync|insufficient/i);
  await readingTip;
  try {
    await assert.rejects(() => client.send(w.unifiedAddress, "0.001", undefined, { signer }), /already in progress/);
    assert.equal(asked, false);
  } finally {
    releaseTip();
    await first;
    tip = async () => 2;
  }

  // One spend at a time across tabs: while another tab holds the spend lock,
  // this tab refuses before building anything. Use a fixture on every Node version.
  const locks = spendLocks();
  Object.defineProperty(globalThis, "navigator", { configurable: true, value: { locks } });
  let release!: () => void;
  const held = new Promise<void>((r) => (release = r));
  let locked!: () => void;
  const acquired = new Promise<void>((r) => (locked = r));
  const other = locks.request("z-stack-wallet-spend", async () => {
    locked();
    await held;
  });
  await acquired;
  try {
    await assert.rejects(() => client.send(w.unifiedAddress, "0.001", undefined, { signer }), /another tab/);
    assert.equal(asked, false);
  } finally {
    release();
    await other;
  }
  await assert.rejects(() => client.send(w.unifiedAddress, "0.001", undefined, { signer }), /sync|insufficient/i);
  assert.equal(asked, false, "releasing the tab lock restores normal pre-device validation");

  // A Ledger account must carry the fingerprint its export produced.
  await assert.rejects(
    () => client.restoreHardware({ ...account, device: "ledger" }, "regtest", 1),
    /fingerprint/i,
  );
});
