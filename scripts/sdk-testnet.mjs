#!/usr/bin/env node
/**
 * SDK integration against public testnet lightwalletd (zec.rocks).
 * Creates a fresh wallet near tip; does not spend unless funded.
 *
 *   Z_STACK_TESTNET=1 pnpm test:sdk:testnet
 *
 * Optional spend (needs a funded mnemonic + birthday):
 *   Z_STACK_TESTNET_MNEMONIC='…' Z_STACK_TESTNET_BIRTHDAY=… Z_STACK_TESTNET_SEND_TO=… Z_STACK_TESTNET_SEND_ZEC=0.0001
 */
import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { createNativeWallet } from "../packages/sdk/src/native.ts";
import { canSend, canShield, parseZecToZatoshis } from "../packages/core/src/index.ts";
import { initialize, parseAddress } from "../packages/sdk/src/lab.ts";
import { ensureZWalletBuilt, spawnServe, stop, waitServeReady, ROOT } from "./sdk-harness.mjs";

if (process.env.Z_STACK_TESTNET !== "1") {
  console.log("skip: set Z_STACK_TESTNET=1 to hit testnet.zec.rocks");
  process.exit(0);
}

function req(name, cond, detail = "") {
  if (!cond) throw new Error(`FAIL ${name}${detail ? `: ${detail}` : ""}`);
  console.log(`ok  ${name}`);
}

const bind = "127.0.0.1:18789";
const url = `http://${bind}`;
let proc;

try {
  await ensureZWalletBuilt();
  const dir = mkdtempSync(join(tmpdir(), "z-sdk-testnet-"));
  proc = spawnServe({ walletDir: dir, bind, passphrase: "testnet-sdk" });
  const auth = await waitServeReady(proc, url);
  const w = createNativeWallet(url, auth);

  const mnemonic = process.env.Z_STACK_TESTNET_MNEMONIC?.trim();
  const birthday = process.env.Z_STACK_TESTNET_BIRTHDAY
    ? Number(process.env.Z_STACK_TESTNET_BIRTHDAY)
    : undefined;

  let snap;
  if (mnemonic) {
    snap = await w.restore(mnemonic, "testnet", birthday);
    req("restore testnet", snap.network === "testnet");
  } else {
    snap = await w.create("testnet", birthday);
    req("create testnet", snap.network === "testnet" && snap.unifiedAddress.startsWith("utest1"));
    req("mnemonic once", typeof snap.mnemonic === "string");
  }

  const wasm = join(ROOT, "packages", "sdk", "src", "generated", "z_wasm_bg.wasm");
  if (existsSync(wasm)) {
    await initialize({ wasmModule: readFileSync(wasm) });
    const parsed = parseAddress(snap.unifiedAddress);
    req("parseAddress", parsed.kind === "unified" && parsed.network === "testnet");
  } else {
    console.log("skip parseAddress (pnpm build:wasm)");
  }

  const tip = await w.waitUntilCaughtUp({ timeoutMs: 180_000, intervalMs: 2_000 });
  req("caught up", tip.behind === 0, JSON.stringify(tip));
  const hist = await w.history();
  req("history", Array.isArray(hist));
  const next = (await w.nextAddress()).unifiedAddress;
  req("next UA", next.startsWith("utest1"));

  const sendTo = process.env.Z_STACK_TESTNET_SEND_TO?.trim();
  const sendZec = process.env.Z_STACK_TESTNET_SEND_ZEC?.trim();
  if (sendTo && sendZec) {
    const after = await w.getWallet();
    if (canShield(after.balance.transparentAvailable)) {
      await w.shield();
      await w.waitUntilCaughtUp({ timeoutMs: 180_000 });
    }
    const funded = await w.getWallet();
    req("can send", canSend(funded.balance, parseZecToZatoshis(sendZec)));
    const sent = await w.send(sendTo, sendZec);
    req("testnet send", Array.isArray(sent.txids) && sent.txids.length > 0);
  } else {
    console.log("skip spend (set Z_STACK_TESTNET_SEND_TO + Z_STACK_TESTNET_SEND_ZEC)");
  }

  console.log("SDK testnet: all checks passed");
} catch (e) {
  console.error(e);
  process.exitCode = 1;
} finally {
  await stop(proc);
}
