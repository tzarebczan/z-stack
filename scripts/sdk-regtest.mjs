#!/usr/bin/env node
import { regtestLwdUrl } from "./regtest-rpc.mjs";
/**
 * SDK integration against local Zebra+Zaino regtest.
 *
 *   pnpm regtest:up
 *   pnpm test:sdk:regtest
 */
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { randomBytes } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createEngineClient } from "../packages/sdk/src/engine.ts";
import { createNativeWallet } from "../packages/sdk/src/native.ts";
import {
  REGTEST_FAUCET_MNEMONIC,
  REGTEST_FAUCET_TRANSPARENT,
} from "../packages/sdk/src/constants.ts";
import { parseZip321, zip321Uri, shieldedAvailableZat, canShield } from "../packages/core/src/index.ts";
import { generate, waitForZebra, zebraRpc, ZEBRA_RPC } from "./regtest-rpc.mjs";
import { assertLocalRegtestChain, ensureZWalletBuilt, spawnServe, stop, waitServeReady } from "./sdk-harness.mjs";

function req(name, cond, detail = "") {
  if (!cond) throw new Error(`FAIL ${name}${detail ? `: ${detail}` : ""}`);
  console.log(`ok  ${name}`);
}

const faucetBind = process.env.Z_STACK_SDK_FAUCET_BIND ?? "127.0.0.1:18787";
const recvBind = process.env.Z_STACK_SDK_RECV_BIND ?? "127.0.0.1:18788";
const faucetUrl = `http://${faucetBind}`;
const recvUrl = `http://${recvBind}`;
const server = regtestLwdUrl();
const setup = { server, validatorRpc: ZEBRA_RPC };
const catchUp = { timeoutMs: 180_000, intervalMs: 500 };
const passphrase = randomBytes(24).toString("hex");

async function waitForIndexedWallet(wallet) {
  const target = Number((await zebraRpc("getblockchaininfo")).blocks);
  const deadline = Date.now() + catchUp.timeoutMs;
  while ((await wallet.tip()).tip < target) {
    if (Date.now() >= deadline) throw new Error("Zaino did not index the mined fixture blocks");
    await new Promise((resolve) => setTimeout(resolve, catchUp.intervalMs));
  }
  return wallet.waitUntilCaughtUp(catchUp);
}

let faucetProc;
let recvProc;
let faucetDir;
let recvDir;

function encryptedOnly(dir) {
  req("encrypted seed exists", existsSync(join(dir, "seed.enc")));
  req("no plaintext mnemonic file", !existsSync(join(dir, "mnemonic.txt")));
}

try {
  console.log("waiting for Zebra…");
  const info = await waitForZebra(20_000);
  assertLocalRegtestChain(info, ZEBRA_RPC);
  req("Zebra matches local regtest activation schedule before mining", true);
  let height = Number(info.blocks) || 0;
  if (height < 110) {
    console.log(`mining to 110 (have ${height})`);
    await generate(110 - height);
    height = 110;
  }

  await ensureZWalletBuilt();
  faucetDir = mkdtempSync(join(tmpdir(), "z-sdk-faucet-"));
  recvDir = mkdtempSync(join(tmpdir(), "z-sdk-recv-"));
  faucetProc = spawnServe({ walletDir: faucetDir, bind: faucetBind, passphrase });
  recvProc = spawnServe({ walletDir: recvDir, bind: recvBind, passphrase });
  const [faucetAuth, recvAuth] = await Promise.all([
    waitServeReady(faucetProc, faucetUrl), waitServeReady(recvProc, recvUrl),
  ]);

  const faucet = createNativeWallet(faucetUrl, faucetAuth);
  let recv = createNativeWallet(recvUrl, recvAuth);
  const raw = createEngineClient(faucetUrl, faucetAuth);

  const h = await faucet.health();
  req(
    "health",
    h.ok === true && h.mode === "native-bridge",
    JSON.stringify(h),
  );

  const probe = await raw.probeSetup("regtest", server, ZEBRA_RPC);
  req("validator matches the checked regtest fixture", probe.validator.ok &&
    probe.validator.chain === info.chain && probe.validator.url.replace(/\/$/, "") === ZEBRA_RPC.replace(/\/$/, ""),
  JSON.stringify(probe.validator));
  // Zaino identifies this local Zebra regtest as "test" in LightdInfo. Only
  // accept that ambiguous label for a loopback fixture with the validator above.
  const localLight = ["127.0.0.1", "localhost", "[::1]"].includes(new URL(server).hostname);
  req("Zaino is local regtest", probe.light.ok && localLight &&
    (probe.light.chain === "regtest" || probe.light.chain === "test"), JSON.stringify(probe.light));
  const restored = await faucet.restore(REGTEST_FAUCET_MNEMONIC, "regtest", 1, setup);
  req("restore faucet", restored.transparentAddress === REGTEST_FAUCET_TRANSPARENT);
  encryptedOnly(faucetDir);
  // Keep sync deterministic; explicit shielding below covers the funding path
  // without an unrelated auto-shield hiding pending state or proving every sync.
  await faucet.setUnlockPolicy("each-spend");
  req("zip321", parseZip321(restored.zip321).address === restored.unifiedAddress);
  req("zip321 uri", zip321Uri(restored.unifiedAddress) === restored.zip321);

  await waitForIndexedWallet(faucet);
  const tip = await faucet.tip();
  req("tip after catch-up", tip.behind === 0 && tip.scanned > 0, JSON.stringify(tip));

  const snap = await faucet.getWallet();
  req("faucet has transparent or orchard", snap.balance.totalAvailable > 0);
  req("canShield", canShield(snap.balance.transparentAvailable) || shieldedAvailableZat(snap.balance) > 0n);

  const hist0 = await faucet.history(20);
  req("history is array", Array.isArray(hist0));

  const next = (await faucet.nextAddress()).unifiedAddress;
  req("next UA", next.startsWith("uregtest1") && next !== snap.unifiedAddress);

  const created = await recv.create("regtest", tip.tip, setup);
  req("create recipient", created.unifiedAddress.startsWith("uregtest1"));
  req("create mnemonic once", typeof created.mnemonic === "string" && created.mnemonic.split(" ").length === 24);
  encryptedOnly(recvDir);
  await recv.setUnlockPolicy("each-spend");
  req("mnemonic not returned again", (await recv.getWallet()).mnemonic === undefined);

  if (snap.balance.transparentAvailable >= 100_000 && shieldedAvailableZat(snap.balance) < 60_000n) {
    const sh = await faucet.shield();
    req("shield txids", Array.isArray(sh.txids) && sh.txids.length > 0, JSON.stringify(sh.txids));
    const preMine = await faucet.getWallet();
    req(
      "shield pending or not-yet-spendable",
      ((preMine.balance.orchardPending ?? 0) + (preMine.balance.ironwoodPending ?? 0)) > 0 || shieldedAvailableZat(preMine.balance) === 0n,
      JSON.stringify(preMine.balance),
    );
    await generate(20);
    await waitForIndexedWallet(faucet);
  }

  const funded = await faucet.getWallet();
  req("mandatory send is funded", shieldedAvailableZat(funded.balance) >= 60_000n, JSON.stringify(funded.balance));
  const fee = await raw.estimateFee(created.unifiedAddress, "0.0005", "sdk-regtest");
  req("engine fee estimate", fee.feeZat > 0 && Number.isSafeInteger(fee.feeZat), JSON.stringify(fee));
  const maximum = await raw.maxSend(created.unifiedAddress);
  req("max-send estimate", maximum.maxSendZat >= 50_000, JSON.stringify(maximum));
  req("shielded recipient address", (await raw.inspectAddress(created.unifiedAddress)).kind === "unified");
  {
    const sent = await faucet.send(created.unifiedAddress, "0.0005", "sdk-regtest");
    req("send txids", Array.isArray(sent.txids) && sent.txids.length > 0);
    const txid = sent.txids[0];
    req("broadcast transaction lookup", (await raw.transaction(txid))?.txid === txid);
    req("pending lifecycle before mining", (await raw.pending()).some((entry) => entry.txid === txid));
    req("history filters before limit", (await raw.history(1, { status: "pending", txid }))[0]?.txid === txid);
    const sentSnap = await faucet.getWallet();
    req(
      "send shows pending change",
      ((sentSnap.balance.orchardPending ?? 0) + (sentSnap.balance.ironwoodPending ?? 0)) > 0 || shieldedAvailableZat(sentSnap.balance) < shieldedAvailableZat(funded.balance),
      JSON.stringify(sentSnap.balance),
    );
    await generate(15);
    await waitForIndexedWallet(faucet);
    await waitForIndexedWallet(recv);
    const rsnap = await recv.getWallet();
    req("recipient funded exactly", shieldedAvailableZat(rsnap.balance) === 50_000n, JSON.stringify(rsnap.balance));
    const rh = await recv.history();
    req("recipient history", rh.length > 0);
    const received = await recv.transaction(txid);
    req("recipient canonical transaction lookup", received?.txid === txid && received.status === "mined");
    req("recipient memo", received?.memos?.includes("sdk-regtest"), JSON.stringify(received?.memos));
    req("sender mined lifecycle", (await raw.transaction(txid))?.status === "mined");
    req("mined transaction no longer pending", !(await raw.pending()).some((entry) => entry.txid === txid));

    const reset = await recv.resetScan();
    req("SDK wipe resets scan", reset.balance.totalAvailable === 0 && reset.birthdayHeight === created.birthdayHeight);
    req("SDK wipe clears history", (await recv.history()).length === 0);
    await waitForIndexedWallet(recv);
    req("SDK rescan restores balance", shieldedAvailableZat((await recv.getWallet()).balance) === 50_000n);
    req("SDK rescan restores transaction", (await recv.transaction(txid))?.status === "mined");

    await stop(recvProc);
    recvProc = spawnServe({ walletDir: recvDir, bind: recvBind, passphrase });
    const restartedAuth = await waitServeReady(recvProc, recvUrl);
    recv = createNativeWallet(recvUrl, restartedAuth);
    req("bridge restart reloads persisted wallet", shieldedAvailableZat((await recv.getWallet()).balance) === 50_000n);
    req("bridge restart retains history", (await recv.transaction(txid))?.status === "mined");
  }

  const methods = [
    "health",
    "getWallet",
    "sync",
    "shield",
    "send",
    "create",
    "restore",
    "history",
    "tip",
    "nextAddress",
    "waitUntilCaughtUp",
    "transaction", "pending", "resetScan", "estimateFee", "maxSend", "inspectAddress",
  ];
  for (const m of methods) req(`EngineClient.${m}`, typeof raw[m] === "function");

  console.log("SDK regtest: all checks passed");
} catch (e) {
  console.error(e);
  process.exitCode = 1;
} finally {
  for (const [child, dir] of [[faucetProc, faucetDir], [recvProc, recvDir]]) {
    try {
      await stop(child);
      if (dir) rmSync(dir, { recursive: true, force: true });
    } catch (error) { console.error("SDK fixture cleanup failed", error); process.exitCode = 1; }
  }
}
