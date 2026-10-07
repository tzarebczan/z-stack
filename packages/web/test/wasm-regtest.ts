import { createWasmClient, deriveAccount, initialize, forgetWasmWallet, peekWasmWallet } from "@z-stack/sdk/lab";
import type { WalletSnapshot } from "@z-stack/core";
import { httpLwdTransport } from "../../sdk/src/lwd";
import { scanWorkerRuntime, scanWorkerStarting, workerScanSession } from "../../sdk/src/scan-host";
import { openWalletDb } from "../../sdk/src/snapshot-storage";

// Deliberately public test fixture; identical to z-engine::keys::REGTEST_FAUCET_MNEMONIC.
const FAUCET = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const CHECKPOINT = "z-stack.wasm-regtest.integration.v1";
const output = document.querySelector<HTMLPreElement>("#results")!;
const button = document.querySelector<HTMLButtonElement>("#run")!;
const mode = new URLSearchParams(location.search).get("mode") === "mt" ? "mt" : "st";

type Checkpoint = {
  phase: "reload";
  mode: "st" | "mt";
  tip: number;
  anchorHash: string;
  address: string;
  balance: WalletSnapshot["balance"];
  historyTxids: string[];
  enhancedTxids: string[];
  blankTxids: string[];
  timings: Record<string, number>;
};

function check(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}
function report(value: unknown) { output.textContent = JSON.stringify(value, null, 2); }
async function timed<T>(name: string, timings: Record<string, number>, fn: () => Promise<T>): Promise<T> {
  const before = performance.now();
  const result = await fn();
  timings[name] = +(performance.now() - before).toFixed(2);
  return result;
}
async function recordCount(): Promise<number> {
  const db = await openWalletDb();
  try {
    return await new Promise<number>((resolve, reject) => {
      const tx = db.transaction("wallets", "readonly");
      const request = tx.objectStore("wallets").getAllKeys();
      // Forget retains non-sensitive generations to reject stale documents.
      // Every other record still blocks fixture creation and fails cleanup.
      tx.oncomplete = () => resolve(request.result.filter(key =>
        !/^wallet:generation$|^vault:(seed|passkey):generation$/.test(String(key))).length);
      tx.onabort = () => reject(tx.error ?? new Error("Fixture IDB read aborted."));
      tx.onerror = () => reject(tx.error);
    });
  } finally { db.close(); }
}

async function run(): Promise<void> {
  button.disabled = true;
  let ownsFixture = false;
  let fixtureAddress = "";
  const timings: Record<string, number> = {};
  try {
    check(location.hostname === "127.0.0.1" && location.port === "15177",
      "Use isolated http://127.0.0.1:15177/test/wasm-regtest.html?mode=st (or mode=mt).");
    const raw = sessionStorage.getItem(CHECKPOINT);
    const checkpoint = raw ? JSON.parse(raw) as Checkpoint : null;
    check(!checkpoint || checkpoint.phase === "reload" && checkpoint.mode === mode, "Mismatched integration checkpoint.");
    if (!checkpoint) check(await recordCount() === 0, "Existing wallet data found; nothing was changed.");
    report({ status: "initializing", mode });
    await timed("initializeMs", timings, () => initialize({ preferMulticore: mode === "mt", threads: mode === "mt" ? 8 : 1,
      prewarmProvingKey: false, prewarmProveWorker: false }));
    await scanWorkerStarting();
    const runtime = scanWorkerRuntime();
    check(runtime, "The real scan worker did not start.");
    check(runtime.mode === (mode === "mt" ? "multi-thread" : "single-thread"), `Unexpected worker mode: ${runtime.mode}`);
    const account = deriveAccount(FAUCET, "regtest", 0);
    fixtureAddress = account.unifiedAddress;
    if (checkpoint) {
      const saved = await peekWasmWallet();
      check(checkpoint.address === fixtureAddress && saved?.network === "regtest"
        && saved.ufvk === account.ufvk && saved.unifiedAddress === fixtureAddress,
      "Reload fixture does not match the public regtest wallet; existing data preserved.");
    }
    const base = httpLwdTransport("http://127.0.0.1:1239", "regtest", "http://127.0.0.1:28137");
    const healthResponse = await fetch("http://127.0.0.1:1239/health");
    const health = await healthResponse.json();
    check(healthResponse.ok && health.network === "regtest" && health.zaino === "http://127.0.0.1:28137",
      "Expected the isolated regtest pipe and Zaino endpoint.");
    const info = await base.info?.();
    // Zebra reports chain="test" for regtest; the loopback pipe carries the
    // explicit network configuration, while LightdInfo alone is ambiguous.
    check(info?.chain === "test" || info?.chain?.toLowerCase().includes("regtest"),
      `Pipe is not a test chain: ${info?.chain ?? "unknown"}`);
    const observedTip = await base.tip();
    check(observedTip > 0 && observedTip < 100_000, `Expected a short isolated regtest chain, got tip ${observedTip}.`);
    const tip = checkpoint?.tip ?? observedTip;
    check(observedTip >= tip, `Regtest tip rewound below the captured height ${tip}.`);
    check(base.treeState, "The regtest transport must expose GetTreeState to anchor the captured tip.");
    const anchor = await base.treeState(tip);
    check(anchor.height === tip && !!anchor.hash, "GetTreeState did not return the captured height/hash.");
    const anchorHash = anchor.hash.toLowerCase();
    if (checkpoint) check(anchorHash === checkpoint.anchorHash, "Captured regtest block changed across reload.");
    const verifyAnchor = async () => {
      check(await base.tip() >= tip, "Regtest tip rewound during the fixture run.");
      const current = await base.treeState!(tip);
      check(current.height === tip && current.hash.toLowerCase() === anchorHash,
        "Captured regtest block changed during the fixture run.");
    };
    const requested: string[] = [];
    const fetched = new Set<string>();
    let blockRequests = 0;
    const transport = {
      ...base, submit: undefined, mine: undefined, mempool: undefined,
      tip: async () => tip,
      utxos: base.utxos ? async (addresses: string[], startHeight?: number) =>
        (await base.utxos!(addresses, startHeight)).filter((utxo) => utxo.height <= tip) : undefined,
      blocks: async (start: number, end: number, signal?: AbortSignal) => {
        blockRequests++;
        return base.blocks(start, end, signal);
      },
      tx: async (txid: string) => {
        requested.push(txid);
        const hex = await base.tx!(txid);
        if (hex) fetched.add(txid);
        return hex;
      },
    };
    const client = createWasmClient({ network: "regtest", transport, lightUrl: "http://127.0.0.1:28137", autoShield: false },
      (progress) => report({ status: checkpoint ? "reload-or-rescan" : "initial-scan", mode, runtime, progress, timings }));
    if (!checkpoint) {
      ownsFixture = true;
      await client.restoreUfvk!(account.ufvk, "regtest", 1);
      const synced = await timed("initialSyncMs", timings, () => client.sync());
      check(synced.scannedHeight === tip, "Initial scan did not reach the fixed tip.");
      check(synced.balance.orchardAvailable > 0, "Fixture has no available Orchard funds; run the native restore/spend gate first.");
      const history = await client.history(500);
      check(history.length > 0, "Fixture history is empty.");
      const unfinished = await workerScanSession()!.memoEnhancementTxids(500);
      check(unfinished !== null, "Rebuild WASM: memo enhancement completion API missing.");
      const enhancedTxids = [...fetched].filter((txid) => !unfinished.includes(txid));
      const blankTxids = enhancedTxids.filter((txid) => !history.find((tx) => tx.txid === txid)?.memos?.length);
      check(blankTxids.length > 0, "Fixture must include a successfully enhanced transaction without text.");
      await verifyAnchor();
      const saved: Checkpoint = { phase: "reload", mode, tip, anchorHash, address: fixtureAddress, balance: synced.balance,
        historyTxids: history.map((tx) => tx.txid).sort(), enhancedTxids, blankTxids, timings };
      sessionStorage.setItem(CHECKPOINT, JSON.stringify(saved));
      report({ status: "reloading", mode, runtime, tip, blockRequests, enhanced: enhancedTxids.length,
        emptyMemoCompletions: blankTxids.length, timings });
      ownsFixture = false; // The next page verifies and then owns this exact fixture.
      location.reload();
      return;
    }

    ownsFixture = true;
    Object.assign(timings, checkpoint.timings);
    const savedPreview = await timed("previewMs", timings, () => peekWasmWallet());
    check(savedPreview?.scannedHeight === tip, "Saved preview height mismatch.");
    await timed("hydrateMs", timings, () => client.tip());
    const hydrated = await timed("catchUpAfterReloadMs", timings, () => client.sync());
    check(blockRequests === 0, "Already-synced reload downloaded compact blocks again.");
    check(JSON.stringify(hydrated.balance) === JSON.stringify(checkpoint.balance), "Balances changed on reload.");
    const history = await client.history(500);
    check(JSON.stringify(history.map((tx) => tx.txid).sort()) === JSON.stringify(checkpoint.historyTxids), "History changed on reload.");
    check(!requested.some((txid) => checkpoint.enhancedTxids.includes(txid)), "Reload fetched an already enhanced transaction again.");
    const reset = await timed("wipeMs", timings, () => client.resetScan!());
    check((reset.scannedHeight ?? 0) <= 1 && reset.balance.totalAvailable === 0, "Wipe did not reset height/balance.");
    check((await peekWasmWallet())?.balance.totalAvailable === 0, "Wipe left the old preview visible.");
    const rescanned = await timed("rescanMs", timings, () => client.sync());
    check(rescanned.scannedHeight === tip, "Rescan did not reach tip.");
    check(JSON.stringify(rescanned.balance) === JSON.stringify(checkpoint.balance), "Balances changed after wipe/rescan.");
    await verifyAnchor();
    const result = { status: "passed", mode, runtime, tip, anchorHash, historyRows: history.length,
      completedEmptyMemosSurvivedReload: checkpoint.blankTxids.length, rescanBlockRequests: blockRequests, timings };
    await forgetWasmWallet();
    check(await recordCount() === 0 && await peekWasmWallet() === null, "Fixture cleanup failed.");
    sessionStorage.removeItem(CHECKPOINT);
    ownsFixture = false;
    report(result);
  } catch (error) {
    report({ status: "failed", mode, message: String(error), timings });
  } finally {
    if (ownsFixture) {
      const current = await peekWasmWallet();
      if (!current || current.network === "regtest" && current.unifiedAddress === fixtureAddress) {
        await forgetWasmWallet();
        sessionStorage.removeItem(CHECKPOINT);
      }
    }
    button.disabled = false;
  }
}
button.onclick = () => { void run(); };
if (sessionStorage.getItem(CHECKPOINT)) void run();
