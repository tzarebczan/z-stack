import {
  createWasmClient, deriveAccount, forgetWasmWallet, initialize, peekWasmWallet,
  REGTEST_FAUCET_MNEMONIC, type EngineClient,
} from "@z-stack/sdk/lab";
import { httpLwdTransport, type BlockTransport } from "../../sdk/src/lwd";
import { openWalletDb } from "../../sdk/src/snapshot-storage";
import { scanWorkerRuntime, scanWorkerStarting, workerScanSession } from "../../sdk/src/scan-host";

const KEY = "z-stack.wasm-prove-regtest.fixture.v1";
const FUNDING_ZAT = 50_000;
const RETURN_ZAT = 5_000;
const output = document.querySelector<HTMLPreElement>("#results")!;
const buttons = Object.fromEntries(["prepare", "receipt", "send", "confirm", "cleanup"].map((id) =>
  [id, document.querySelector<HTMLButtonElement>(`#${id}`)!]));
type Phase = "preparing" | "await-funding" | "receipt-reload" | "ready-to-send" | "proving" | "await-mining";
type Fixture = {
  id: string; phase: Phase; address: string; ufvk: string; birthday: number;
  initialTip: number; initialHash: string; receivedHeight?: number; receiptTxids?: string[];
  beforeSpend?: number; txid?: string; broadcastAttempted?: boolean; feeEstimate?: number;
  proofWorkerRequests?: number; proofWorkerReplies?: number; timings: Record<string, number>;
};
let fixture: Fixture | null = null;
let client: EngineClient | null = null;
let transport: BlockTransport | null = null;
let initialization: Promise<void> | null = null;
let busy = false;
let broadcastArmed = false;
const workerEvidence = { scanApplies: 0, proofRequests: 0, proofReplies: 0 };

function check(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}
function guardOrigin() {
  check(location.hostname === "127.0.0.1" && location.port === "15179",
    "Use only isolated http://127.0.0.1:15179/test/wasm-prove-regtest.html.");
}
function save() { check(fixture, "No fixture checkpoint."); sessionStorage.setItem(KEY, JSON.stringify(fixture)); }
function render(status: string, extra: Record<string, unknown> = {}) {
  // Intentionally project a small public result; never serialize an SDK create
  // result, wallet snapshot, worker request, session storage, or a seed.
  output.textContent = JSON.stringify({ status, phase: fixture?.phase, address: fixture?.address,
    birthday: fixture?.birthday, initialTip: fixture?.initialTip, fundingZat: FUNDING_ZAT, returnZat: RETURN_ZAT,
    receiptTxids: fixture?.receiptTxids, returnTxid: fixture?.txid, timings: fixture?.timings,
    workerEvidence, ...extra }, null, 2);
  buttons.prepare.disabled = busy || !!fixture;
  buttons.receipt.disabled = busy || fixture?.phase !== "await-funding";
  buttons.send.disabled = busy || fixture?.phase !== "ready-to-send" || !!fixture.broadcastAttempted;
  buttons.confirm.disabled = busy || fixture?.phase !== "await-mining";
  buttons.cleanup.disabled = busy || !fixture;
}
async function timed<T>(key: string, action: () => Promise<T>): Promise<T> {
  const start = performance.now();
  const result = await action();
  if (fixture) { fixture.timings[key] = +(performance.now() - start).toFixed(2); save(); }
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
async function assertOwned() {
  check(fixture?.address && fixture.ufvk, "No complete fixture identity; existing data was preserved.");
  const saved = await peekWasmWallet();
  check(saved?.network === "regtest" && saved.unifiedAddress === fixture.address && saved.ufvk === fixture.ufvk,
    "Saved wallet does not match this fixture; existing data was preserved.");
}

// Observe only operation names/counts. Worker payloads include spending secrets
// and must never be retained in test output or instrumentation.
const BrowserWorker = window.Worker;
class ObservedWorker extends BrowserWorker {
  private proofIds = new Set<number>();
  constructor(url: string | URL, options?: WorkerOptions) {
    super(url, options);
    this.addEventListener("message", (event: MessageEvent<{ id?: number; error?: unknown }>) => {
      if (typeof event.data?.id === "number" && this.proofIds.delete(event.data.id) && !event.data.error) {
        workerEvidence.proofReplies++;
      }
    });
  }
  override postMessage(message: unknown, options?: Transferable[] | StructuredSerializeOptions): void {
    const op = message as { id?: number; kind?: string; op?: string } | null;
    if (op?.op === "applyBlob") workerEvidence.scanApplies++;
    if (op?.kind === "send" && typeof op.id === "number") {
      this.proofIds.add(op.id);
      workerEvidence.proofRequests++;
    }
    if (Array.isArray(options)) super.postMessage(message, options);
    else super.postMessage(message, options);
  }
}
window.Worker = ObservedWorker;

async function init() {
  if (initialization) return initialization;
  initialization = (async () => {
    guardOrigin();
    await initialize({ preferMulticore: false, threads: 1, prewarmProveWorker: true, prewarmProvingKey: false });
    await scanWorkerStarting();
    check(scanWorkerRuntime()?.mode === "single-thread", "A real ST scan worker is required.");
    const base = httpLwdTransport("http://127.0.0.1:1239", "regtest", "http://127.0.0.1:28137");
    transport = { ...base, mine: undefined, submit: async (hex) => {
      check(broadcastArmed && fixture?.phase === "proving" && !fixture.broadcastAttempted,
        "Broadcast is permitted only by the explicit prove-and-submit button, once per fixture.");
      check(workerEvidence.proofRequests === 1 && workerEvidence.proofReplies === 1,
        "A successful proof from the real prove worker is required before broadcast.");
      broadcastArmed = false;
      fixture.broadcastAttempted = true;
      fixture.proofWorkerRequests = workerEvidence.proofRequests;
      fixture.proofWorkerReplies = workerEvidence.proofReplies;
      save(); // An interrupted/ambiguous submit must never silently retry on reload.
      check(base.submit, "Regtest pipe cannot submit transactions.");
      const txid = await base.submit(hex);
      check(/^[0-9a-f]{64}$/i.test(txid), "Node accepted submit but returned no canonical txid; inspect the fixture before retrying.");
      fixture.txid = txid.toLowerCase();
      fixture.phase = "await-mining";
      save();
      return txid;
    } };
    client = createWasmClient({ network: "regtest", transport, lightUrl: "http://127.0.0.1:28137", autoShield: false },
      (progress) => render("working", { progress: { stage: progress.stage, scanned: progress.scanned, tip: progress.tip } }));
    await assertChain();
  })().catch((error) => { initialization = null; throw error; });
  return initialization;
}
async function assertChain() {
  const response = await fetch("http://127.0.0.1:1239/health", { signal: AbortSignal.timeout(5000) });
  const health = await response.json();
  check(response.ok && health.network === "regtest" && health.zaino === "http://127.0.0.1:28137",
    "Expected the dedicated regtest pipe connected to local Zaino28137.");
  check(transport?.treeState && transport.info, "Fixture needs GetTreeState and GetLightdInfo.");
  const info = await transport.info();
  check(info.chain === "test" || info.chain === "regtest", "Unexpected lightwallet chain.");
  const tip = await transport.tip();
  check(tip > 0 && tip < 100_000, "Expected the short disposable regtest chain.");
  if (fixture?.initialHash) {
    check(tip >= fixture.initialTip, "Fixture chain rewound below its captured tip.");
    const anchor = await transport.treeState(fixture.initialTip);
    check(anchor.height === fixture.initialTip && anchor.hash.toLowerCase() === fixture.initialHash,
      "Captured chain changed; fixture preserved for inspection.");
  }
  return tip;
}

async function prepare() {
  guardOrigin();
  check(!fixture && !sessionStorage.getItem(KEY) && await recordCount() === 0,
    "Existing wallet or fixture found; nothing was changed.");
  await init();
  const tip = await assertChain();
  const anchor = await transport!.treeState!(tip);
  check(anchor.height === tip && !!anchor.hash, "Unable to anchor the fixture's initial chain.");
  const created = await client!.create("regtest", 1, { passkey: false });
  // The SDK keeps its seed in this browser session; discard the returned copy.
  delete created.mnemonic;
  check(created.ufvk, "Created wallet has no viewing key.");
  fixture = { id: crypto.randomUUID(), phase: "preparing", address: created.unifiedAddress, ufvk: created.ufvk,
    birthday: created.birthdayHeight, initialTip: tip, initialHash: anchor.hash.toLowerCase(), timings: {} };
  save();
  const empty = await timed("noteFreeScanMs", () => client!.sync());
  check(empty.balance.totalAvailable === 0 && (empty.balance.totalPending ?? 0) === 0,
    "Fresh fixture unexpectedly has funds.");
  check((await client!.history()).length === 0 && workerEvidence.scanApplies > 0,
    "Fresh fixture must scan existing unrelated history before its first receipt.");
  check(await workerScanSession()!.sinsemillaLive(), "Note-free scan was not finalized.");
  await assertOwned();
  fixture.phase = "await-funding";
  save();
  render("awaiting coordinator funding", { instruction: "Fund the displayed address with 50,000 zatoshi, mine at least 10 blocks, then click Scan funded receipt and reload." });
}

async function receipt() {
  check(fixture?.phase === "await-funding", "Fixture is not awaiting funding.");
  await init(); await assertOwned(); await assertChain();
  const received = await timed("firstReceiptScanMs", () => client!.sync());
  check(received.balance.orchardAvailable === FUNDING_ZAT && (received.balance.totalPending ?? 0) === 0,
    `Expected exactly ${FUNDING_ZAT} confirmed Orchard zatoshi; fund/mine first.`);
  const history = await client!.history(500);
  check(history.length > 0 && history.every((tx) => tx.status === "mined" && (tx.minedHeight ?? 0) > fixture!.initialTip),
    "Receipt history must be mined after the captured note-free scan.");
  fixture.receiptTxids = history.map((tx) => tx.txid).sort();
  fixture.receivedHeight = received.scannedHeight;
  fixture.beforeSpend = received.balance.orchardAvailable;
  fixture.phase = "receipt-reload";
  save();
  render("receipt saved; reloading the real wallet snapshot");
  location.reload();
}

async function resume() {
  guardOrigin();
  const raw = sessionStorage.getItem(KEY);
  if (!raw) { render("ready; no fixture created"); return; }
  fixture = JSON.parse(raw) as Fixture;
  check(fixture && typeof fixture.id === "string" && fixture.birthday === 1 && /^uregtest1/.test(fixture.address),
    "Invalid fixture checkpoint; data preserved.");
  await init(); await assertOwned(); await assertChain();
  await timed("reloadHydrateMs", () => client!.tip());
  if (fixture.phase === "receipt-reload") {
    const wallet = await client!.sync();
    check(wallet.balance.orchardAvailable === fixture.beforeSpend && client!.hasSpendingSeed(),
      "Saved receipt or session-only spending seed did not survive reload.");
    check(JSON.stringify((await client!.history(500)).map((tx) => tx.txid).sort()) === JSON.stringify(fixture.receiptTxids),
      "Receipt history changed across reload.");
    fixture.phase = "ready-to-send"; save();
  }
  render(fixture.phase === "ready-to-send" ? "ready for coordinator-authorized proof and submit" : "fixture resumed",
    fixture.phase === "proving" ? { instruction: "Interrupted proof/submit; inspect before retrying. No automatic broadcast will occur." }
      : { restoredHistoryTxids: (await client!.history(500)).map((tx) => tx.txid).sort() });
}

async function send() {
  check(fixture?.phase === "ready-to-send" && !fixture.broadcastAttempted, "Fixture is not ready for one authorized spend.");
  await init(); await assertOwned(); await assertChain();
  const to = deriveAccount(REGTEST_FAUCET_MNEMONIC, "regtest", 0).unifiedAddress;
  const fee = await client!.estimateFee(to, "0.00005", "wasm-worker-proof-regtest");
  check((fixture.beforeSpend ?? 0) > RETURN_ZAT + fee.feeZat, "Fixture cannot cover the return and fee.");
  fixture.feeEstimate = fee.feeZat;
  fixture.phase = "proving"; save();
  const requests = workerEvidence.proofRequests;
  const replies = workerEvidence.proofReplies;
  broadcastArmed = true;
  try {
    await timed("proveAndNodeSubmitMs", () => client!.send(to, "0.00005", "wasm-worker-proof-regtest"));
  } finally { broadcastArmed = false; }
  check(workerEvidence.proofRequests === requests + 1 && workerEvidence.proofReplies === replies + 1,
    "Expected successful send proof from the actual prove worker.");
  fixture.proofWorkerRequests = workerEvidence.proofRequests - requests;
  fixture.proofWorkerReplies = workerEvidence.proofReplies - replies;
  check(fixture.txid && (await client!.transaction(fixture.txid))?.status === "pending",
    "Accepted transaction is missing from pending SDK history.");
  save();
  render("node accepted the worker-proved transaction; awaiting coordinator mining", { feeEstimateZat: fee.feeZat,
    instruction: "Mine at least 10 blocks, then click Confirm mined return and clean up." });
}

async function confirm() {
  check(fixture?.phase === "await-mining" && fixture.txid, "No accepted return transaction to confirm.");
  await init(); await assertOwned(); await assertChain();
  const wallet = await timed("confirmMinedReturnMs", () => client!.sync());
  const mined = await client!.transaction(fixture.txid);
  check(mined?.status === "mined" && mined.minedHeight && mined.feeZat !== null,
    "Return transaction is not mined yet; the coordinator must mine first.");
  check(!(await client!.pending()).some((tx) => tx.txid === fixture!.txid), "Mined return remained pending.");
  const expected = fixture.beforeSpend! - RETURN_ZAT - mined.feeZat;
  check(wallet.balance.orchardAvailable === expected && (wallet.balance.totalPending ?? 0) === 0,
    "Confirmed change does not equal funding minus return and actual fee.");
  check(fixture.proofWorkerRequests === 1 && fixture.proofWorkerReplies === 1, "Worker proof evidence is incomplete.");
  const result = { status: "passed", returnTxid: fixture.txid, minedHeight: mined.minedHeight,
    actualFeeZat: mined.feeZat, remainingOrchardZat: expected, timings: fixture.timings,
    proofWorkerRequests: fixture.proofWorkerRequests, proofWorkerReplies: fixture.proofWorkerReplies };
  await cleanup();
  render("passed; fixture wallet and session seed removed", result);
}

async function cleanup() {
  guardOrigin(); await assertOwned();
  await forgetWasmWallet();
  check(await recordCount() === 0 && await peekWasmWallet() === null, "Fixture cleanup did not remove its wallet records.");
  sessionStorage.removeItem(KEY);
  fixture = null;
  client = null;
  initialization = null;
  render("fixture removed");
}
async function run(action: () => Promise<void>) {
  if (busy) return;
  busy = true; render("working");
  try { await action(); }
  catch (error) { render("failed; fixture preserved", { message: String(error) }); }
  finally { busy = false; for (const button of Object.values(buttons)) button.disabled = false;
    // Update controls without overwriting the final public result.
    buttons.prepare.disabled = !!fixture;
    buttons.receipt.disabled = fixture?.phase !== "await-funding";
    buttons.send.disabled = fixture?.phase !== "ready-to-send" || !!fixture?.broadcastAttempted;
    buttons.confirm.disabled = fixture?.phase !== "await-mining";
    buttons.cleanup.disabled = !fixture;
  }
}
buttons.prepare.onclick = () => void run(prepare);
buttons.receipt.onclick = () => void run(receipt);
buttons.send.onclick = () => void run(send);
buttons.confirm.onclick = () => void run(confirm);
buttons.cleanup.onclick = () => void run(cleanup);
void run(resume);
