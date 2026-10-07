/**
 * Wallet scan session: in-process (Node / tests) or a dedicated Worker (browser).
 * Apply runs off the UI thread so fetch + paint keep moving during catch-up.
 */

import {
  runHardware,
  type HardwareAction,
  type HardwareArgs,
  type HardwareResult,
  type HardwareStatics,
  type HardwareWalletHandle,
} from "./hardware-ops";
import { trackScanRevision } from "./scan-revision";

export type HardwareAccountInput = {
  device: "keystone" | "ledger";
  ufvk: string;
  seedFingerprint: string;
  accountIndex: number;
};

export type ScanRuntime = {
  mode: "single-thread" | "multi-thread";
  threads: number;
  sharedArrayBuffer: boolean;
  crossOriginIsolated: boolean;
  orchardCircuit: boolean;
  simd: boolean;
  scanWorker?: boolean;
};

export type ApplySummary = {
  notesFound: number;
  spendsFound: number;
  scanned: number;
};

export type ScanSession = {
  readonly kind?: "local" | "worker";
  create: (network: string, mnemonic: string, birthday: number, accountIndex: number) => Promise<void>;
  fromUfvk: (network: string, ufvk: string, birthday: number, accountIndex: number) => Promise<void>;
  fromHardware: (network: string, account: HardwareAccountInput, birthday: number) => Promise<void>;
  fromSnapshot: (bytes: Uint8Array) => Promise<void>;
  /** Hardware-wallet engine calls (see `hardware-ops`). */
  hardware: (action: HardwareAction, args?: HardwareArgs) => Promise<HardwareResult>;
  applyBlob: (blob: Uint8Array, transparent?: boolean) => Promise<ApplySummary>;
  applyTreeState: (json: string) => Promise<void>;
  applySubtreeRoots: (protocol: string, json: string) => Promise<number>;
  applyUtxos: (json: string) => Promise<number>;
  applyMempool: (json: string) => Promise<number>;
  enhanceRawTx: (hex: string) => Promise<number>;
  /** Probe the underlying generated WASM, not just this session's wrapper. */
  supportsTransparentBlocks?: () => Promise<boolean>;
  applyTransparentBlocks?: (blob: Uint8Array) => Promise<number>;
  applySharedMemos?: (json: string) => Promise<number>;
  /** null only for older generated WASM without persisted enhancement state. */
  memoEnhancementTxids: (limit: number) => Promise<string[] | null>;
  rewindTo: (height: number) => Promise<number>;
  resetScan: () => Promise<void>;
  rescanFrom: (birthday: number) => Promise<void>;
  recomputePools: (onTick?: (p: { hashed: number; total: number; message: string }) => void) => Promise<void>;
  nextUnifiedAddress: () => Promise<void>;
  attachSeed: (mnemonic: string) => Promise<void>;
  estimateFee: (to: string, amountZec: string, memo?: string) => Promise<string>;
  estimateTransparentFee?: (to: string, amountZec: string) => Promise<string>;
  supportsTransparentSend?: () => Promise<boolean>;
  maxSend: (to?: string) => Promise<string>;
  snapshotJson: (server: string) => Promise<string>;
  toSnapshot: () => Promise<Uint8Array>;
  persistenceSnapshot: () => Promise<{ bytes: Uint8Array; previewJson: string }>;
  /** Conservative session revision; undefined while a mutation is in flight. */
  persistenceRevision?: () => number | undefined;
  history: (limit: number) => Promise<string>;
  /** Raw hex of unmined, unexpired sends, for an idempotent rebroadcast. */
  pendingRawTxs: () => Promise<string[]>;
  scannedHeight: () => Promise<number>;
  birthday: () => Promise<number>;
  nextHeight: () => Promise<number>;
  treesReady: () => Promise<boolean>;
  /** In-memory shardtrees already hashed; follow-on must not re-finalize. */
  sinsemillaLive: () => Promise<boolean>;
  /** `null` when this wasm build cannot report stored shard counts. */
  subtreeRootCounts: () => Promise<{ sapling: number; orchard: number; ironwood: number } | null>;
  /** First shard to request per pool (birthday shard on a fresh restore); `null` on older wasm. */
  subtreeRootsStart: () => Promise<{ sapling: number; orchard: number; ironwood: number } | null>;
  transparentAddress: () => Promise<string | undefined>;
  forget: () => Promise<void>;
  /**
   * Multicore scan worker only: prove on the worker's own wallet, whose trees
   * are built, using its Rayon pool. The wallet keeps the pending
   * transaction. Other sessions prove in the single-threaded prove worker.
   */
  prove?: (
    kind: "send" | "sendTransparent" | "shield",
    mnemonic: string,
    args: { to?: string; amountZec?: string; memo?: string; maxFeeZat?: string; thresholdZat?: number },
  ) => Promise<{ hex: string; txid?: string }>;
  /** Build the Orchard proving key in the instance that proves (see `prove`). */
  warmProvingKey?: () => Promise<{ ready: boolean; ms: number }>;
};

type WalletHandle = HardwareWalletHandle & {
  free?: () => void;
  applyCompactBlocksSummary?: (blob: Uint8Array) => string;
  applyCompactBlocks: (blob: Uint8Array) => string;
  applyTreeState?: (json: string) => void;
  applySubtreeRoots?: (protocol: string, json: string) => number;
  applyUtxos?: (json: string) => number;
  applyMempool?: (json: string) => number;
  enhanceRawTx?: (hex: string) => number;
  applyTransparentBlocks?: (blob: Uint8Array) => number;
  applySharedMemos?: (json: string) => number;
  memoEnhancementTxids?: (limit: number) => string;
  rewindTo?: (height: number) => number;
  resetScan?: () => void;
  rescanFrom?: (birthday: number) => void;
  recomputePools?: () => void;
  recomputePoolsWithTick?: (
    cb: (hashed: number, total: number, message: string) => void,
  ) => void;
  nextUnifiedAddress: () => string;
  attachSeed?: (mnemonic: string) => void;
  estimateFee?: (to: string, amountZec: string, memo?: string) => string;
  estimateTransparentFee?: (to: string, amountZec: string) => string;
  proveTransparentSend?: (mnemonic: string, to: string, amountZec: string, maxFeeZat?: string) => string;
  maxSend?: (to?: string) => string;
  snapshotJson: (server: string) => string;
  toSnapshot: () => Uint8Array;
  history: (limit: number) => string;
  pendingRawTxs?: () => string;
  scannedHeight: () => number;
  birthday: () => number;
  nextHeight: () => number;
  treesReady?: () => boolean;
  sinsemillaLive?: () => boolean;
  subtreeRootCount?: (protocol: string) => number;
  subtreeRootsStart?: (protocol: string) => number;
  transparentAddress: () => string | undefined;
};

function parseRawTxs(json: unknown): string[] {
  if (typeof json !== "string") return [];
  try {
    const list: unknown = JSON.parse(json);
    return Array.isArray(list) ? list.filter((hex): hex is string => typeof hex === "string") : [];
  } catch {
    return [];
  }
}

export function localScanSession(
  handle: WalletHandle,
  revive?: (bytes: Uint8Array) => WalletHandle,
  statics: HardwareStatics = {},
): ScanSession {
  let current: WalletHandle | null = handle;
  const w = () => {
    if (!current) throw new Error("no wasm wallet");
    return current;
  };
  return trackScanRevision({
    kind: "local",
    create: async () => {
      throw new Error("local session is created via WasmWallet.create");
    },
    fromUfvk: async () => {
      throw new Error("local session is created via WasmWallet.fromUfvk");
    },
    fromHardware: async () => {
      throw new Error("local session is created via WasmWallet.fromHardware");
    },
    hardware: async (action, args = {}) => runHardware(w(), statics, action, args),
    fromSnapshot: async (bytes) => {
      w(); // Explicitly forgotten handles cannot be revived by a late rollback.
      if (!revive) throw new Error("local session cannot revive snapshot");
      const next = revive(bytes);
      current?.free?.();
      current = next;
    },
    applyBlob: async (blob, transparent = false) => {
      try {
        if (transparent && !w().applyTransparentBlocks) throw new Error("WASM needs a public-data upgrade");
        let notesFound = 0;
        let spendsFound = 0;
        if (typeof w().applyCompactBlocksSummary === "function") {
          const s = JSON.parse(w().applyCompactBlocksSummary!(blob)) as {
            notesFound?: number;
            spendsFound?: number;
          };
          notesFound = s.notesFound ?? 0;
          spendsFound = s.spendsFound ?? 0;
        } else {
          const deltas = JSON.parse(w().applyCompactBlocks(blob)) as Array<{
            notesFound?: number;
            spendsFound?: number;
          }>;
          for (const d of deltas) {
            notesFound += d.notesFound ?? 0;
            spendsFound += d.spendsFound ?? 0;
          }
        }
        if (transparent) {
          const wallet = w();
          if (!wallet.applyTransparentBlocks) throw new Error("WASM needs a public-data upgrade");
          wallet.applyTransparentBlocks(blob);
        }
        return { notesFound, spendsFound, scanned: w().scannedHeight() };
      } catch (e) {
        throw new Error(formatScanWorkerError(e instanceof Error ? e.message : String(e)));
      }
    },
    applyTreeState: async (json) => {
      try {
        w().applyTreeState?.(json);
      } catch (e) {
        throw new Error(formatScanWorkerError(e instanceof Error ? e.message : String(e)));
      }
    },
    applySubtreeRoots: async (protocol, json) => {
      try {
        return w().applySubtreeRoots?.(protocol, json) ?? 0;
      } catch (e) {
        throw new Error(formatScanWorkerError(e instanceof Error ? e.message : String(e)));
      }
    },
    applyUtxos: async (json) => w().applyUtxos?.(json) ?? 0,
    applyMempool: async (json) => w().applyMempool?.(json) ?? 0,
    enhanceRawTx: async (hex) => w().enhanceRawTx?.(hex) ?? 0,
    supportsTransparentBlocks: async () => typeof w().applyTransparentBlocks === "function",
    applyTransparentBlocks: async (blob) => {
      const wallet = w();
      if (!wallet.applyTransparentBlocks) throw new Error("WASM needs a public-data upgrade");
      return wallet.applyTransparentBlocks(blob);
    },
    applySharedMemos: async (json) => {
      const wallet = w();
      if (!wallet.applySharedMemos) throw new Error("WASM needs a public-data upgrade");
      return wallet.applySharedMemos(json);
    },
    memoEnhancementTxids: async (limit) => {
      const cur = w();
      return cur.memoEnhancementTxids ? JSON.parse(cur.memoEnhancementTxids(limit)) as string[] : null;
    },
    rewindTo: async (height) => w().rewindTo?.(height) ?? 0,
    rescanFrom: async (birthday) => {
      const cur = w();
      if (!cur.rescanFrom) throw new Error("this wasm build cannot rescan an earlier birthday");
      cur.rescanFrom(birthday);
    },
    resetScan: async () => {
      const cur = w();
      if (typeof cur.resetScan !== "function") throw new Error("this wasm build cannot reset scan");
      cur.resetScan();
    },
    recomputePools: async (onTick) => {
      try {
        const cur = w();
        if (typeof cur.recomputePoolsWithTick === "function") {
          cur.recomputePoolsWithTick((hashed, total, message) => {
            onTick?.({ hashed, total, message });
          });
        } else {
          cur.recomputePools?.();
        }
      } catch (e) {
        throw new Error(formatScanWorkerError(e instanceof Error ? e.message : String(e)));
      }
    },
    nextUnifiedAddress: async () => {
      w().nextUnifiedAddress();
    },
    attachSeed: async (mnemonic) => {
      const cur = w();
      if (typeof cur.attachSeed !== "function") throw new Error("attachSeed missing");
      cur.attachSeed(mnemonic);
    },
    estimateFee: async (to, amountZec, memo) => {
      const cur = w();
      if (typeof cur.estimateFee !== "function") throw new Error("this wasm build cannot estimate fees");
      return cur.estimateFee(to, amountZec, memo);
    },
    supportsTransparentSend: async () => typeof w().estimateTransparentFee === "function" && typeof w().proveTransparentSend === "function",
    estimateTransparentFee: async (to, amountZec) => {
      const cur = w();
      if (typeof cur.estimateTransparentFee !== "function") throw new Error("this wasm build cannot estimate transparent swap outputs");
      return cur.estimateTransparentFee(to, amountZec);
    },
    maxSend: async (to) => {
      const cur = w();
      if (typeof cur.maxSend !== "function") throw new Error("this wasm build cannot estimate max send");
      return cur.maxSend(to);
    },
    snapshotJson: async (server) => w().snapshotJson(server),
    toSnapshot: async () => w().toSnapshot(),
    persistenceSnapshot: async () => {
      const cur = w();
      // No await between serialization and preview: both describe one wallet state.
      const bytes = cur.toSnapshot();
      return { bytes, previewJson: cur.snapshotJson("") };
    },
    history: async (limit) => w().history(limit),
    pendingRawTxs: async () => parseRawTxs(w().pendingRawTxs?.()),
    scannedHeight: async () => w().scannedHeight(),
    birthday: async () => w().birthday(),
    nextHeight: async () => w().nextHeight(),
    treesReady: async () => !!w().treesReady?.(),
    sinsemillaLive: async () => !!w().sinsemillaLive?.(),
    subtreeRootCounts: async () => {
      const cur = w();
      if (typeof cur.subtreeRootCount !== "function") return null;
      return {
        sapling: cur.subtreeRootCount("sapling"),
        orchard: cur.subtreeRootCount("orchard"),
        ironwood: cur.subtreeRootCount("ironwood"),
      };
    },
    subtreeRootsStart: async () => {
      const cur = w();
      if (typeof cur.subtreeRootsStart !== "function") return null;
      return {
        sapling: cur.subtreeRootsStart("sapling"),
        orchard: cur.subtreeRootsStart("orchard"),
        ironwood: cur.subtreeRootsStart("ironwood"),
      };
    },
    transparentAddress: async () => w().transparentAddress(),
    forget: async () => {
      current?.free?.();
      current = null;
    },
  });
}

export function canUseScanWorker(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof Worker !== "undefined" &&
    typeof document !== "undefined"
  );
}

let worker: Worker | null = null;
let workerRuntime: ScanRuntime | null = null;
let seq = 1;
let rpcGen = 0;
let inflight = 0;
let rpcTail: Promise<void> = Promise.resolve();
let attachTail: Promise<boolean> = Promise.resolve(false);
let spawnWorker: (() => Worker) | null = null;
let lastOpts: { threads: number; preferMulticore: boolean; wasmBasePath?: string } | null = null;
let currentOp = "";
let failedWorker = false;
let removeWorkerListeners: (() => void) | null = null;

const RESTART_FOR = new Set(["applyBlob", "fromSnapshot", "create", "fromUfvk", "fromHardware"]);
let attaching: Promise<(ScanRuntime & { scanWorker: true }) | null> | null = null;

export function isTreeConflictError(msg: string): boolean {
  return /selective-scan (insert_tree|tree conflict)|Inserted root conflicts|Wipe scan & resync/i.test(
    msg,
  );
}

export function treeConflictUserMessage(raw: string): string {
  const msg = raw.trim();
  if (!isTreeConflictError(msg)) return msg;
  if (/Wipe scan & resync/i.test(msg)) return msg;
  return `${msg} Wipe scan & resync (or rewind to last good mark). Do not keep scanning or send until trees rebuild.`;
}

function formatScanWorkerError(raw: unknown): string {
  return treeConflictUserMessage(typeof raw === "string" ? raw : String(raw));
}

type PendingRpc = {
  finish: (fn: () => void) => void;
  reject: (error: Error) => void;
};
const pending = new Map<number, PendingRpc>();

function abortAll(reason: string): void {
  const err = new Error(reason);
  for (const p of [...pending.values()]) {
    p.finish(() => p.reject(err));
  }
  pending.clear();
}

function dispatchRpc(
  op: string,
  payload: Record<string, unknown>,
  transfer: Transferable[] | undefined,
  timeoutMs: number,
  onProgress?: (p: Record<string, unknown>) => void,
): Promise<Record<string, unknown>> {
  const w = worker;
  if (!w) return Promise.reject(new Error("no scan worker"));
  const generation = rpcGen;
  const id = seq++;
  return new Promise((resolve, reject) => {
    let settled = false;
    const finish = (fn: () => void) => {
      if (settled) return;
      settled = true;
      pending.delete(id);
      w.removeEventListener("message", onMsg);
      if (timer) clearTimeout(timer);
      fn();
    };
    const onMsg = (ev: MessageEvent) => {
      if (ev.data?.id !== id) return;
      if (ev.data?.progress) {
        onProgress?.(ev.data as Record<string, unknown>);
        return;
      }
      if (ev.data.error) finish(() => reject(new Error(formatScanWorkerError(ev.data.error))));
      else finish(() => resolve(ev.data as Record<string, unknown>));
    };
    const timer =
      timeoutMs > 0
        ? setTimeout(() => {
            if (w === worker && generation === rpcGen) {
              retireScanWorker(`scan worker ${op} timed out after ${timeoutMs}ms`, true);
            }
          }, timeoutMs)
        : undefined;
    pending.set(id, { finish, reject });
    w.addEventListener("message", onMsg);
    try {
      if (transfer?.length) w.postMessage({ id, op, ...payload }, transfer);
      else w.postMessage({ id, op, ...payload });
    } catch (e) {
      if (w === worker && generation === rpcGen) {
        retireScanWorker(e instanceof Error ? e.message : String(e), true);
      } else finish(() => reject(e));
    }
  });
}

function rpc(
  op: string,
  payload: Record<string, unknown> = {},
  transfer?: Transferable[],
  timeoutMs = 60_000,
  onProgress?: (p: Record<string, unknown>) => void,
): Promise<Record<string, unknown>> {
  const g = rpcGen;
  inflight++;
  const run = () => {
    if (g !== rpcGen) return Promise.reject(new Error("scan worker restarted"));
    currentOp = op;
    return dispatchRpc(op, payload, transfer, timeoutMs, onProgress).finally(() => {
      if (g === rpcGen && currentOp === op) currentOp = "";
    });
  };
  const p = rpcTail.then(run, run);
  rpcTail = p.then(
    () => {},
    () => {},
  );
  void p.catch(() => {});
  const settled = () => {
    if (g === rpcGen) inflight = Math.max(0, inflight - 1);
  };
  void p.then(settled, settled);
  return p;
}

export function scanWorkerBusy(): boolean {
  return inflight > 0;
}

/** Retire exactly this worker generation, rejecting queued and active RPCs. */
function retireScanWorker(reason = "scan worker restarted", failed = false): void {
  removeWorkerListeners?.();
  removeWorkerListeners = null;
  failedWorker = failed;
  rpcGen += 1;
  inflight = 0;
  currentOp = "";
  abortAll(reason);
  rpcTail = Promise.resolve();
  try { worker?.terminate(); } catch { /* already stopped */ }
  worker = null;
  workerRuntime = null;
  attaching = null;
  attachTail = Promise.resolve(false);
  seq = 1;
}

/** A failed generation can be recovered by a later explicit saved-wallet load. */
export function scanWorkerFailed(): boolean { return failedWorker; }

/** Reuse pending initialization; never replace another caller's healthy worker. */
export function recoverScanWorker(): Promise<(ScanRuntime & { scanWorker: true }) | null> {
  if (workerRuntime) return Promise.resolve({ ...workerRuntime, scanWorker: true });
  if (attaching) return attaching;
  if (!failedWorker || !spawnWorker || !lastOpts) return Promise.resolve(null);
  return attachScanWorker(spawnWorker(), lastOpts, spawnWorker);
}

/** Kill the worker (cancels applyBlob). Next attach/restart loads wasm again. */
export async function restartScanWorker(
  spawn?: () => Worker,
): Promise<(ScanRuntime & { scanWorker: true }) | null> {
  const make = spawn ?? spawnWorker;
  retireScanWorker();
  if (!make || !lastOpts) return null;
  spawnWorker = make;
  return attachScanWorker(make(), lastOpts, make);
}

/**
 * Drop the in-memory wallet before create/restore. If a snapshot load or compact-block
 * apply is running, terminate — those ops cannot be cancelled and would eat the create timeout.
 */
export async function prepareScanWorkerForNewWallet(spawn?: () => Worker): Promise<void> {
  if (!worker) return;
  const original = worker;
  const generation = rpcGen;
  const current = () => original === worker && generation === rpcGen;
  if (inflight > 0 && RESTART_FOR.has(currentOp)) {
    await restartScanWorker(spawn);
    return;
  }
  if (inflight > 0) {
    try {
      await rpcTail;
    } catch {
      /* worker going idle */
    }
  }
  if (!current()) throw new Error("scan worker restarted");
  try {
    await rpc("forget", {}, undefined, 8_000);
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    if (!current() || /scan worker restarted/i.test(msg)) throw e;
    await restartScanWorker(spawn);
  }
}

export async function attachScanWorker(
  spawned: Worker,
  opts: {
    threads: number;
    preferMulticore: boolean;
    wasmBasePath?: string;
    regtestNu63Height?: number;
  },
  spawn?: () => Worker,
): Promise<(ScanRuntime & { scanWorker: true }) | null> {
  if (worker && workerRuntime) {
    try {
      if (spawned !== worker) spawned.terminate();
    } catch {
      /* already have one */
    }
    return { ...workerRuntime, scanWorker: true };
  }
  if (worker && attaching) {
    if (spawned !== worker) spawned.terminate();
    return attaching;
  }
  lastOpts = { ...opts };
  if (spawn) spawnWorker = spawn;
  const work = attachScanWorkerInner(spawned, opts);
  attaching = work;
  const release = () => { if (attaching === work) attaching = null; };
  void work.then(release, release);
  attachTail = work.then(
    (wr) => !!wr,
    () => false,
  );
  return work;
}

async function attachScanWorkerInner(
  spawned: Worker,
  opts: {
    threads: number;
    preferMulticore: boolean;
    wasmBasePath?: string;
    regtestNu63Height?: number;
  },
): Promise<(ScanRuntime & { scanWorker: true }) | null> {
  worker = spawned;
  failedWorker = false;
  const generation = rpcGen;
  const current = () => worker === spawned && generation === rpcGen;
  const onFailure = () => {
    if (current()) retireScanWorker(`scan worker ${currentOp || "idle"} failed`, true);
  };
  spawned.addEventListener("error", onFailure);
  spawned.addEventListener("messageerror", onFailure);
  removeWorkerListeners = () => {
    spawned.removeEventListener("error", onFailure);
    spawned.removeEventListener("messageerror", onFailure);
  };
  try {
    const data = await rpc(
      "init",
      {
        preferMulticore: opts.preferMulticore,
        threads: opts.threads,
        wasmBasePath: opts.wasmBasePath,
        ...(opts.regtestNu63Height ? { regtestNu63Height: opts.regtestNu63Height } : {}),
      },
      undefined,
      90_000,
    );
    if (!current()) return null;
    workerRuntime = {
      mode: data.mode === "multi-thread" ? "multi-thread" : "single-thread",
      threads: Number(data.threads) || 1,
      sharedArrayBuffer: !!data.sharedArrayBuffer,
      crossOriginIsolated: !!data.crossOriginIsolated,
      orchardCircuit: !!data.orchardCircuit,
      simd: !!data.simd,
      scanWorker: true,
    };
    return { ...workerRuntime, scanWorker: true };
  } catch {
    if (current()) console.warn("scan worker failed; apply stays on this thread");
    if (current()) retireScanWorker("scan worker init failed", true);
    return null;
  }
}

export function scanWorkerReady(): boolean {
  return !!workerRuntime;
}

/** Includes a worker whose initialization is still pending. */
export function scanWorkerPresent(): boolean { return worker !== null; }

/** Resolves when the in-flight scan-worker init finishes (or immediately if none). */
export function scanWorkerStarting(): Promise<boolean> {
  if (workerRuntime) return Promise.resolve(true);
  return attachTail;
}

export function scanWorkerRuntime(): ScanRuntime | null {
  return workerRuntime;
}

let sessionWorker: Worker | null = null;
let sessionGeneration = -1;
let cachedWorkerSession: ScanSession | null = null;

export function workerScanSession(): ScanSession | null {
  if (!worker || !workerRuntime) return null;
  if (sessionWorker === worker && sessionGeneration === rpcGen && cachedWorkerSession) return cachedWorkerSession;
  sessionWorker = worker;
  sessionGeneration = rpcGen;
  const ownedWorker = worker;
  const generation = rpcGen;
  const current = () => ownedWorker === worker && generation === rpcGen;
  const sessionRpc = (...args: Parameters<typeof rpc>) => current()
    ? rpc(...args) : Promise.reject(new Error("scan worker restarted"));
  const multicore = workerRuntime.mode === "multi-thread";
  return cachedWorkerSession = trackScanRevision({
    kind: "worker",
    create: async (network, mnemonic, birthday, accountIndex) => {
      await sessionRpc("create", { network, mnemonic, birthday, accountIndex }, undefined, 90_000);
    },
    fromUfvk: async (network, ufvk, birthday, accountIndex) => {
      await sessionRpc("fromUfvk", { network, ufvk, birthday, accountIndex }, undefined, 90_000);
    },
    fromHardware: async (network, account, birthday) => {
      await sessionRpc("fromHardware", { network, birthday, ...account }, undefined, 90_000);
    },
    hardware: async (action, args = {}) => {
      const transfer: Transferable[] = [];
      const own = (b?: Uint8Array) => {
        if (!b) return undefined;
        const copy = b.slice();
        transfer.push(copy.buffer);
        return copy.buffer;
      };
      const { pczt, signed, ...rest } = args;
      // Proving can take a while; the rest are quick.
      const data = await sessionRpc(
        "hardware",
        { action, ...rest, pczt: own(pczt), signed: own(signed) },
        transfer,
        action === "prove" ? 600_000 : 60_000,
      );
      const buf = data.bytes as ArrayBuffer | Uint8Array | undefined;
      return {
        ...(buf ? { bytes: buf instanceof Uint8Array ? buf : new Uint8Array(buf) } : {}),
        ...(typeof data.json === "string" ? { json: data.json } : {}),
        ...(typeof data.n === "number" ? { n: data.n } : {}),
      };
    },
    fromSnapshot: async (bytes) => {
      const copy =
        bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength
          ? bytes
          : bytes.slice();
      // Transfer the IDB buffer — do not clone 60MB again. A timeout retires
      // this worker; only a later explicit wallet load may restore saved bytes.
      await sessionRpc("fromSnapshot", { snapshot: copy.buffer }, [copy.buffer], 180_000);
    },
    applyBlob: async (blob, transparent = false) => {
      const buf =
        blob.byteOffset === 0 && blob.byteLength === blob.buffer.byteLength
          ? blob.buffer
          : blob.slice().buffer;
      const data = await sessionRpc("applyBlob", { blob: buf, transparent }, [buf], 0);
      return {
        notesFound: Number(data.notesFound) || 0,
        spendsFound: Number(data.spendsFound) || 0,
        scanned: Number(data.scanned) || 0,
      };
    },
    applyTreeState: async (json) => {
      await sessionRpc("applyTreeState", { json });
    },
    applySubtreeRoots: async (protocol, json) => {
      const data = await sessionRpc("applySubtreeRoots", { protocol, json }, undefined, 180_000);
      return Number(data.n) || 0;
    },
    applyUtxos: async (json) => {
      const data = await sessionRpc("applyUtxos", { json });
      return Number(data.n) || 0;
    },
    applyMempool: async (json) => {
      const data = await sessionRpc("applyMempool", { json });
      return Number(data.n) || 0;
    },
    supportsTransparentBlocks: async () => (await sessionRpc("meta")).transparentCompact === true,
    applyTransparentBlocks: async (blob) => {
      const buf = blob.slice().buffer as ArrayBuffer;
      return Number((await sessionRpc("applyTransparentBlocks", { blob: buf }, [buf], 0)).n) || 0;
    },
    applySharedMemos: async (json) => {
      return Number((await sessionRpc("applySharedMemos", { json }, undefined, 180_000)).n) || 0;
    },
    enhanceRawTx: async (hex) => {
      const data = await sessionRpc("enhanceRawTx", { hex });
      return Number(data.n) || 0;
    },
    memoEnhancementTxids: async (limit) => {
      const data = await sessionRpc("memoEnhancementTxids", { limit });
      return data.json == null ? null : JSON.parse(String(data.json)) as string[];
    },
    rewindTo: async (height) => {
      const data = await sessionRpc("rewindTo", { height });
      return Number(data.height) || 0;
    },
    rescanFrom: async (birthday) => { await sessionRpc("rescanFrom", { birthday }); },
    resetScan: async () => {
      await sessionRpc("resetScan");
    },
    recomputePools: async (onTick) => {
      await sessionRpc("recomputePools", {}, undefined, 600_000, (p) => {
        onTick?.({
          hashed: Number(p.hashed) || 0,
          total: Number(p.total) || 0,
          message: String(p.message ?? ""),
        });
      });
    },
    nextUnifiedAddress: async () => {
      await sessionRpc("nextUnifiedAddress");
    },
    attachSeed: async (mnemonic) => {
      await sessionRpc("attachSeed", { mnemonic });
    },
    estimateFee: async (to, amountZec, memo) => {
      const data = await sessionRpc("estimateFee", { to, amountZec, memo });
      return String(data.json ?? "{}");
    },
    supportsTransparentSend: async () => !!(await sessionRpc("supportsTransparentSend")).supported,
    estimateTransparentFee: async (to, amountZec) => {
      const data = await sessionRpc("estimateTransparentFee", { to, amountZec });
      return String(data.json ?? "{}");
    },
    maxSend: async (to) => {
      const data = await sessionRpc("maxSend", { to });
      return String(data.json ?? "{}");
    },
    snapshotJson: async (server) => {
      const data = await sessionRpc("snapshotJson", { server });
      return String(data.json ?? "{}");
    },
    toSnapshot: async () => {
      const data = await sessionRpc("toSnapshot", {}, undefined, 180_000);
      const buf = data.snapshot as ArrayBuffer | Uint8Array;
      return buf instanceof Uint8Array ? buf : new Uint8Array(buf);
    },
    persistenceSnapshot: async () => {
      const data = await sessionRpc("persistenceSnapshot", {}, undefined, 180_000);
      const buf = data.snapshot as ArrayBuffer | Uint8Array;
      return { bytes: buf instanceof Uint8Array ? buf : new Uint8Array(buf), previewJson: String(data.json) };
    },
    history: async (limit) => {
      const data = await sessionRpc("history", { limit });
      return String(data.json ?? "[]");
    },
    scannedHeight: async () => {
      const data = await sessionRpc("meta");
      return Number(data.scanned) || 0;
    },
    birthday: async () => {
      const data = await sessionRpc("meta");
      return Number(data.birthday) || 1;
    },
    nextHeight: async () => {
      const data = await sessionRpc("meta");
      return Number(data.nextHeight) || 1;
    },
    pendingRawTxs: async () => parseRawTxs((await sessionRpc("pendingRawTxs")).json),
    treesReady: async () => {
      const data = await sessionRpc("meta");
      return !!data.treesReady;
    },
    sinsemillaLive: async () => {
      const data = await sessionRpc("meta");
      return !!data.sinsemillaLive;
    },
    subtreeRootCounts: async () => {
      const data = await sessionRpc("meta");
      if (!data.hasIncrementalRoots) return null;
      return {
        sapling: Number(data.saplingRoots) || 0,
        orchard: Number(data.orchardRoots) || 0,
        ironwood: Number(data.ironwoodRoots) || 0,
      };
    },
    subtreeRootsStart: async () => {
      const data = await sessionRpc("meta");
      if (!data.hasRootsStart) return null;
      return {
        sapling: Number(data.saplingRootsStart) || 0,
        orchard: Number(data.orchardRootsStart) || 0,
        ironwood: Number(data.ironwoodRootsStart) || 0,
      };
    },
    transparentAddress: async () => {
      const data = await sessionRpc("meta");
      return (data.transparentAddress as string | undefined) || undefined;
    },
    forget: async () => {
      await sessionRpc("forget");
    },
    ...(multicore
      ? {
          prove: async (
            kind: "send" | "sendTransparent" | "shield",
            mnemonic: string,
            args: { to?: string; amountZec?: string; memo?: string; maxFeeZat?: string; thresholdZat?: number },
          ) => {
            // Same limit as the prove worker; a first send may also build the key.
            const data = await sessionRpc("prove", { kind, mnemonic, ...args }, undefined, 600_000);
            return { hex: String(data.hex ?? ""), ...(typeof data.txid === "string" ? { txid: data.txid } : {}) };
          },
          warmProvingKey: async () => {
            const data = await sessionRpc("warmProvingKey", {}, undefined, 600_000);
            return { ready: !!data.ready, ms: Number(data.ms) || 0 };
          },
        }
      : {}),
  });
}

/** Interrupt only the captured session; never retire a successor's worker. */
export function interruptScanWorkerSession(expected: ScanSession): boolean {
  if (workerScanSession() !== expected) return false;
  retireScanWorker("wallet proof cancelled before broadcast");
  return true;
}

export async function forgetScanWorkerWallet(): Promise<void> {
  retireScanWorker();
}
