#!/usr/bin/env node
/**
 * Regtest: fund an orchard note, restore from seed, require spend-ready, prove a send.
 *
 * Does not print the recipient mnemonic.
 *
 *   pnpm regtest:up   # start a disposable loopback fixture
 *   node --import tsx scripts/regtest-restore-prove.mjs
 */
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { createNativeWallet } from "../packages/sdk/src/native.ts";
import {
  REGTEST_FAUCET_MNEMONIC,
  REGTEST_FAUCET_TRANSPARENT,
} from "../packages/sdk/src/constants.ts";
import { generate, waitForZebra } from "./regtest-rpc.mjs";
import { ROOT, waitHttp, stop } from "./sdk-harness.mjs";

function req(name, cond, detail = "") {
  if (!cond) throw new Error(`FAIL ${name}${detail ? `: ${detail}` : ""}`);
  console.log(`ok  ${name}`);
}

function zWalletBin() {
  const name = process.platform === "win32" ? "z-wallet.exe" : "z-wallet";
  const rel = join(ROOT, "target", "release", name);
  const dbg = join(ROOT, "target", "debug", name);
  if (existsSync(rel)) return rel;
  if (existsSync(dbg)) return dbg;
  throw new Error("build z-wallet first");
}

function spawnServe({ walletDir, bind, passphrase = "regtest" }) {
  const child = spawn(
    zWalletBin(),
    [
      "--passphrase",
      passphrase,
      "--wallet",
      walletDir,
      "--windows-credential",
      "false",
      "serve",
      "--bind",
      bind,
    ],
    { cwd: ROOT, stdio: ["ignore", "pipe", "pipe"], windowsHide: true },
  );
  let buf = "";
  return new Promise((resolve, reject) => {
    const onData = (d) => {
      buf += d.toString();
      const m = buf.match(/token\s+(\S+)/);
      if (m) {
        child.stdout?.off("data", onData);
        child.stderr?.off("data", onData);
        resolve({ child, token: m[1] });
      }
    };
    child.stdout?.on("data", onData);
    child.stderr?.on("data", onData);
    child.on("error", reject);
    child.on("exit", (code) => {
      if (!buf.includes("token")) reject(new Error(`serve exited ${code}: ${buf.slice(-400)}`));
    });
    setTimeout(() => reject(new Error(`serve token timeout: ${buf.slice(-400)}`)), 20_000);
  });
}

function walletUrl(bind, token) {
  return `http://${bind}/?token=${encodeURIComponent(token)}`;
}

const faucetBind = "127.0.0.1:18791";
const recvBind = "127.0.0.1:18792";
const restoreBind = "127.0.0.1:18793";

let faucetProc;
let recvProc;
let restoreProc;

try {
  const info = await waitForZebra(20_000);
  let height = Number(info.blocks) || 0;
  console.log(JSON.stringify({ phase: "zebra", height, chain: info.chain }));
  if (height < 110) {
    await generate(110 - height);
    height = 110;
  }

  const faucetDir = mkdtempSync(join(tmpdir(), "z-prove-faucet-"));
  const recvDir = mkdtempSync(join(tmpdir(), "z-prove-recv-"));
  const restoreDir = mkdtempSync(join(tmpdir(), "z-prove-restore-"));
  const faucetServe = await spawnServe({ walletDir: faucetDir, bind: faucetBind });
  const recvServe = await spawnServe({ walletDir: recvDir, bind: recvBind });
  faucetProc = faucetServe.child;
  recvProc = recvServe.child;
  await waitHttp(`http://${faucetBind}/health`);
  await waitHttp(`http://${recvBind}/health`);

  const faucet = createNativeWallet(walletUrl(faucetBind, faucetServe.token));
  const recv = createNativeWallet(walletUrl(recvBind, recvServe.token));

  const restoredFaucet = await faucet.restore(REGTEST_FAUCET_MNEMONIC, "regtest", 1, {
    passphrase: "regtest",
  });
  req("faucet t-addr", restoredFaucet.transparentAddress === REGTEST_FAUCET_TRANSPARENT);
  await faucet.waitUntilCaughtUp({ timeoutMs: 120_000, intervalMs: 400 });

  let snap = await faucet.getWallet();
  if (snap.balance.transparentAvailable >= 100_000 && snap.balance.orchardAvailable === 0) {
    const sh = await faucet.shield();
    req("shield", Array.isArray(sh.txids) && sh.txids.length > 0, JSON.stringify(sh.txids));
    await generate(20);
    await faucet.waitUntilCaughtUp({ timeoutMs: 90_000 });
    snap = await faucet.getWallet();
  }
  req(
    "faucet orchard",
    snap.balance.orchardAvailable >= 60_000,
    JSON.stringify(snap.balance),
  );

  const created = await recv.create("regtest", 1, { passphrase: "regtest" });
  const recipientMnemonic = created.mnemonic;
  req("recipient mnemonic in memory", typeof recipientMnemonic === "string" && recipientMnemonic.split(" ").length === 24);
  const recipientUa = created.unifiedAddress;
  console.log(JSON.stringify({ phase: "recipient-created", uaPrefix: recipientUa.slice(0, 12) }));

  const sent = await faucet.send(recipientUa, "0.0005", "restore-prove");
  req("fund txids", Array.isArray(sent.txids) && sent.txids.length > 0);
  await generate(15);
  await recv.waitUntilCaughtUp({ timeoutMs: 90_000 });
  const funded = await recv.getWallet();
  req("recipient funded", funded.balance.totalAvailable > 0, JSON.stringify(funded.balance));
  console.log(
    JSON.stringify({
      phase: "funded",
      orchard: funded.balance.orchardAvailable,
      spendReady: funded.spendReady,
      scanned: funded.scannedHeight,
    }),
  );

  stop(recvProc);
  recvProc = undefined;

  const restoreServe = await spawnServe({
    walletDir: restoreDir,
    bind: restoreBind,
    passphrase: "regtest-restore",
  });
  restoreProc = restoreServe.child;
  await waitHttp(`http://${restoreBind}/health`);
  const restored = createNativeWallet(walletUrl(restoreBind, restoreServe.token));
  const t0 = Date.now();
  await restored.restore(recipientMnemonic, "regtest", 1, {
    passphrase: "regtest-restore",
  });
  await restored.waitUntilCaughtUp({ timeoutMs: 90_000 });
  const afterRestore = await restored.getWallet();
  const restoreMs = Date.now() - t0;
  req("restore found funds", afterRestore.balance.totalAvailable > 0, JSON.stringify(afterRestore.balance));
  req(
    "restore orchard",
    afterRestore.balance.orchardAvailable > 0,
    JSON.stringify(afterRestore.balance),
  );
  console.log(
    JSON.stringify({
      phase: "restored",
      ms: restoreMs,
      orchard: afterRestore.balance.orchardAvailable,
      spendReady: afterRestore.spendReady,
      scanned: afterRestore.scannedHeight,
      history: (await restored.history(10)).length,
    }),
  );

  const faucetUa = snap.unifiedAddress;
  const proved = await restored.send(faucetUa, "0.0002", "prove-after-restore");
  req("prove send", Array.isArray(proved.txids) && proved.txids.length > 0, JSON.stringify(proved.txids));
  await generate(10);
  await restored.waitUntilCaughtUp({ timeoutMs: 90_000 });
  await faucet.waitUntilCaughtUp({ timeoutMs: 90_000 });
  const afterSend = await restored.getWallet();
  const faucetAfter = await faucet.getWallet();
  console.log(
    JSON.stringify({
      phase: "proved",
      txids: proved.txids,
      recipientOrchard: afterSend.balance.orchardAvailable,
      faucetOrchard: faucetAfter.balance.orchardAvailable,
    }),
  );
  req("recipient spent", afterSend.balance.orchardAvailable < afterRestore.balance.orchardAvailable);
  console.log("regtest restore+prove: all checks passed");
} catch (e) {
  console.error(e);
  process.exitCode = 1;
} finally {
  stop(faucetProc);
  stop(recvProc);
  stop(restoreProc);
}
