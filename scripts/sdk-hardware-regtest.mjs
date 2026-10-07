#!/usr/bin/env node
/**
 * Hardware-wallet send through the browser SDK (createWasmClient) on the
 * native regtest stack, with a simulated Keystone: the "device" signs the
 * redacted PCZT with the account's phrase, which a real Keystone holds.
 *
 *   pnpm regtest:native:up && node scripts/grpc-web-proxy.mjs &
 *   wasm-pack build crates/z-wasm --features simulator
 *   node --conditions=@z-stack/source --import tsx scripts/sdk-hardware-regtest.mjs
 *
 * Funds the hardware account from the regtest faucet first when it is empty.
 * Regtest only: the validator and light server must be loopback.
 */
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { generate, zebraRpc, ZEBRA_RPC } from "./regtest-rpc.mjs";
import { assertLocalRegtestChain } from "./sdk-harness.mjs";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const LIGHT = process.env.Z_STACK_GRPC_WEB ?? "http://127.0.0.1:28138";
const NU63 = Number(process.env.Z_STACK_REGTEST_NU6_3 || 150);
// Public BIP-39 test vector; regtest funds only.
const HW_PHRASE = `${"abandon ".repeat(23)}art`;

const sdk = await import(join(ROOT, "packages/sdk/src/lab.ts"));
const { createWasmClient } = await import(join(ROOT, "packages/sdk/src/wasm-client.ts"));
const { grpcWebTransport } = await import(join(ROOT, "packages/sdk/src/lwd.ts"));
const { keystoneSigner, hardwareCancelled } = await import(join(ROOT, "packages/sdk/src/hardware.ts"));
const { ledgerWasm } = await import(join(ROOT, "packages/sdk/src/runtime.ts"));

let failures = 0;
const check = (ok, label) => {
  console.log(`${ok ? "PASS" : "FAIL"} ${label}`);
  if (!ok) failures++;
};

if (!/^http:\/\/(127\.0\.0\.1|localhost)(:|\/)/.test(LIGHT)) throw new Error("regtest light server must be loopback");
assertLocalRegtestChain(await zebraRpc("getblockchaininfo"), ZEBRA_RPC);
await sdk.initialize({
  wasmModule: readFileSync(join(ROOT, "packages/sdk/src/generated/z_wasm_bg.wasm")),
  prewarmProvingKey: false,
  prewarmProveWorker: false,
  regtestNu63Height: NU63,
});

const transport = grpcWebTransport(LIGHT, { transparent: true });
const client = createWasmClient({ network: "regtest", transport, autoShield: false });
const mine = async (n = 1) => {
  await generate(n);
  // Let the light server index the new block before syncing.
  const target = (await zebraRpc("getblockchaininfo")).blocks;
  for (let i = 0; i < 100 && (await transport.tip()) < target; i++) await new Promise((r) => setTimeout(r, 200));
  await client.sync();
};
const spendable = (w) => w.balance.orchardAvailable + (w.balance.ironwoodAvailable ?? 0);
const hwAccount = sdk.deriveAccount(HW_PHRASE, "regtest", 0);
const account = {
  device: "keystone",
  ufvk: hwAccount.ufvk,
  seedFingerprint: sdk.seedFingerprint(HW_PHRASE),
  accountIndex: 0,
};

let w = await client.restoreHardware(account, "regtest", 1);
w = await client.sync();
if (spendable(w) < 20_000_000) {
  console.log("funding the hardware account from the regtest faucet…");
  await client.restore(sdk.REGTEST_FAUCET_MNEMONIC, "regtest", 1);
  let f = await client.sync();
  if (spendable(f) < 30_000_000) {
    await client.shield(100_000);
    await mine(1);
    f = await client.getWallet();
  }
  await client.send(hwAccount.unifiedAddress, "0.5", "fund hardware");
  await mine(1);
  w = await client.restoreHardware(account, "regtest", 1);
  w = await client.sync();
}
check(w.hardware?.device === "keystone" && !w.viewOnly, "restored as a Keystone account");
check(spendable(w) >= 20_000_000, `hardware account has shielded funds (${spendable(w)} zat)`);

// Cancel on the device: nothing is broadcast and the notes are free again.
let earlier;
const cancel = keystoneSigner(async (pczt) => {
  earlier = pczt;
  throw hardwareCancelled("user backed out");
});
const to = sdk.deriveAccount(sdk.REGTEST_FAUCET_MNEMONIC, "regtest", 0).unifiedAddress;
try {
  await client.send(to, "0.01", undefined, { signer: cancel });
  check(false, "a cancelled signing rejects");
} catch (e) {
  check(e?.code === "hardware_cancelled", `a cancelled signing rejects with hardware_cancelled (${e?.code})`);
}
check(spendable(await client.getWallet()) === spendable(w), "cancelling released the reserved notes");

// A device answering with signatures for another transaction (a replayed or
// forged answer) is caught before anything is broadcast.
const deviceSign = (pczt) => {
  const sign = ledgerWasm().simulateDeviceSigning;
  if (typeof sign !== "function") {
    throw new Error("rebuild z-wasm with --features simulator to run the hardware regtest");
  }
  return sign(pczt, HW_PHRASE, "regtest", 0);
};
const replay = keystoneSigner(async () => deviceSign(earlier));
try {
  await client.send(to, "0.01", undefined, { signer: replay });
  check(false, "signatures for another transaction are refused");
} catch (e) {
  check(e?.code === "hardware_mismatch", `signatures for another transaction are refused (${e?.code}: ${String(e?.message).slice(0, 200)})`);
}
check(spendable(await client.getWallet()) === spendable(w), "and the notes are released");

// The real flow.
const stages = [];
const keystone = keystoneSigner(async (pczt) => deviceSign(pczt));
const t0 = performance.now();
const sent = await client.send(to, "0.0123", "signed on a (simulated) Keystone", {
  signer: keystone,
  onStage: (s) => {
    stages.push(s);
    // A throwing progress callback must not fail (or half-finish) the send.
    if (s === "broadcasting") throw new Error("UI bug in onStage");
  },
});
const ms = Math.round(performance.now() - t0);
check(/^[0-9a-f]{64}$/.test(sent.txid ?? ""), `broadcast ${sent.txid} in ${ms} ms`);
check(stages.join(",") === "connecting,reviewing,proving,broadcasting", `stages: ${stages.join(",")}`);
const pending = await client.history(5);
check(pending.some((h) => h.txid === sent.txid), "the send is in history while pending");
await mine(1);
const mined = (await client.history(5)).find((h) => h.txid === sent.txid);
check(!!mined && mined.status !== "pending" && mined.minedHeight, `mined at ${mined?.minedHeight}`);

console.log(failures ? `${failures} FAILED` : "ALL PASSED");
process.exit(failures ? 1 : 0);
