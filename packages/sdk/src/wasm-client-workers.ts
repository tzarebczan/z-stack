import {
  attachScanWorker,
  canUseScanWorker,
  treeConflictUserMessage,
  workerScanSession,
  type ScanSession
} from "./scan-host";

import { runtimeState } from './wasm-client-coordination';
import type { Bindings } from './wasm-client-runtime';

export type ProveResult = { hex: string; txid?: string; snapshot?: Uint8Array };

export type ProveWorkerSession = {
  pending: Set<(error: unknown) => void>;
  fail: (error: unknown) => void;
};

export const proveWorkerSessions = new WeakMap<Worker, ProveWorkerSession>();

/** Optional regtest NU7 height shared by worker instances (see `initialize`). */
export function configureRegtestNu7Height(height?: number): void {
  runtimeState.workerRegtestNu7 = height;
}

export function configureRegtestNu63Height(height?: number): void {
  runtimeState.workerRegtestNu63 = height;
}

/** Initialization owns one artifact source for keys, scan and proving workers. */
export function configureWasmWorkerBasePath(basePath?: string): void {
  if (runtimeState.proveWorkerStarted && runtimeState.workerWasmBasePath !== basePath) {
    throw new Error("cannot change wasmBasePath after the proving worker has started");
  }
  runtimeState.workerWasmBasePath = basePath;
}

export function canUseProveWorker(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof Worker !== "undefined" &&
    typeof document !== "undefined"
  );
}

export function getProveWorker(): Worker | null {
  if (!runtimeState.proveWorker) {
    if (!canUseProveWorker()) return null;
    try {
      const worker = new Worker(new URL("./prove.worker.ts", import.meta.url), { type: "module" });
      const pending: ProveWorkerSession["pending"] = new Set();
      let failed = false;
      const fail = (error: unknown) => {
        if (failed) return;
        failed = true;
        worker.removeEventListener("error", onError);
        worker.removeEventListener("messageerror", onError);
        // An event queued by a retired worker must not invalidate its successor.
        if (runtimeState.proveWorker === worker) {
          runtimeState.proveWorker = undefined;
          runtimeState.provePrewarm = null;
          runtimeState.workerProvingKeyReady = false;
        }
        for (const reject of pending) reject(error);
        try { worker.terminate(); } catch { /* already unavailable */ }
      };
      const onError = () => fail(new Error("prove worker failed"));
      worker.addEventListener("error", onError);
      worker.addEventListener("messageerror", onError);
      proveWorkerSessions.set(worker, { pending, fail });
      runtimeState.proveWorker = worker;
      runtimeState.proveWorkerStarted = true;
    } catch {
      // A later explicit request may retry construction; never replay work.
      return null;
    }
  }
  return runtimeState.proveWorker;
}

export function prewarmProveWorker(): void {
  if (!getProveWorker() || runtimeState.provePrewarm) return;
  const preload = workerCall({ kind: "init" }).catch((_e) => {
    if (runtimeState.provePrewarm === preload) runtimeState.provePrewarm = null;
    console.warn("prove worker preload failed");
  });
  runtimeState.provePrewarm = preload;
}

export function spawnScanWorker(): Worker {
  return new Worker(new URL("./scan.worker.ts", import.meta.url), { type: "module" });
}

/** Scan worker URL must live in this module so Vite emits `?worker_file`. */
export async function startScanWorker(opts: {
  threads: number;
  preferMulticore: boolean;
  wasmBasePath?: string;
  regtestNu63Height?: number;
  regtestNu7Height?: number;
}): Promise<Awaited<ReturnType<typeof attachScanWorker>>> {
  if (!canUseScanWorker()) return null;
  try {
    return await attachScanWorker(spawnScanWorker(), opts, spawnScanWorker);
  } catch {
    console.warn("scan worker spawn failed");
    return null;
  }
}

export function workerCall(payload: Record<string, unknown>, transfer?: Transferable[]): Promise<MessageEvent["data"]> {
  const w = getProveWorker();
  if (!w) return Promise.reject(new Error("no prove worker"));
  const session = proveWorkerSessions.get(w)!;
  const id = runtimeState.proveSeq++;
  return new Promise((resolve, reject) => {
    const cleanup = () => {
      clearTimeout(timer);
      w.removeEventListener("message", onMsg);
      session.pending.delete(fail);
    };
    const onMsg = (ev: MessageEvent) => {
      if (ev.data?.id !== id) return;
      cleanup();
      if (ev.data.error) reject(new Error(treeConflictUserMessage(String(ev.data.error))));
      else resolve(ev.data);
    };
    const fail = (error: unknown) => { cleanup(); reject(error); };
    // A blocked proof also blocks the worker's queued jobs. Retire that session
    // together so a late result cannot be mistaken for a reusable worker.
    const timer = setTimeout(() => session.fail(new Error("prove worker timed out")), 600_000);
    w.addEventListener("message", onMsg);
    session.pending.add(fail);
    try {
      const regtest = {
        ...(runtimeState.workerRegtestNu63 ? { regtestNu63Height: runtimeState.workerRegtestNu63 } : {}),
        ...(runtimeState.workerRegtestNu7 ? { regtestNu7Height: runtimeState.workerRegtestNu7 } : {}),
      };
      w.postMessage({ id, wasmBasePath: runtimeState.workerWasmBasePath, ...regtest, ...payload }, transfer ?? []);
    } catch (e) {
      session.fail(e);
    }
  });
}

export function proveInWorker(req: Omit<ProveResult, "hex" | "txid" | "snapshot"> & {
  snapshot: Uint8Array;
  kind: "send" | "sendTransparent" | "shield";
  mnemonic: string;
  to?: string;
  amountZec?: string;
  maxFeeZat?: string;
  memo?: string;
  thresholdZat?: number;
}): Promise<ProveResult> {
  const copy = req.snapshot.slice();
  return workerCall({ ...req, snapshot: copy.buffer }, [copy.buffer]).then((data) => ({
    hex: data.hex as string,
    ...(typeof data.txid === "string" ? { txid: data.txid } : {}),
    snapshot: data.snapshot as Uint8Array,
  }));
}

/**
 * Build the Orchard proving key off the UI thread (prove worker) when possible.
 * Concurrent requests share one build per worker. On Node / tests, runs on this thread.
 */
export async function prewarmOrchardProvingKey(): Promise<{ ready: boolean; ms: number }> {
  const start = Date.now();
  // A multicore scan worker proves itself, so its instance needs the key.
  const scan = workerScanSession();
  const worker = scan?.warmProvingKey ? null : getProveWorker();
  const target = scan?.warmProvingKey ? scan : worker ?? mainThreadProvingKeyTarget;
  if (orchardProvingKeyReady()) return { ready: true, ms: 0 };
  const pending = provingKeyWarmTasks.get(target);
  if (pending) return pending;
  const task = warmProvingKeyFor(scan, worker, start);
  provingKeyWarmTasks.set(target, task);
  try {
    return await task;
  } finally {
    if (provingKeyWarmTasks.get(target) === task) provingKeyWarmTasks.delete(target);
  }
}

export const mainThreadProvingKeyTarget = {};

export const provingKeyWarmTasks = new WeakMap<object, Promise<{ ready: boolean; ms: number }>>();

export async function warmProvingKeyFor(
  scan: ScanSession | null,
  w: Worker | null,
  start: number,
): Promise<{ ready: boolean; ms: number }> {
  if (scan?.warmProvingKey) {
    try {
      const warmed = await scan.warmProvingKey();
      if (warmed.ready && workerScanSession() === scan) runtimeState.scanWorkerKeyFor = scan;
      return warmed;
    } catch {
      return { ready: false, ms: Date.now() - start };
    }
  }
  if (w) {
    try {
      const data = await workerCall({ kind: "warm" });
      if (runtimeState.proveWorker !== w) return { ready: false, ms: Date.now() - start };
      runtimeState.workerProvingKeyReady = !!data.ready;
      return { ready: runtimeState.workerProvingKeyReady, ms: typeof data.ms === "number" ? data.ms : Date.now() - start };
    } catch {
      // A failed browser worker must not move a long key build onto the UI.
      return { ready: false, ms: Date.now() - start };
    }
  }
  try {
    const g = globalThis as unknown as { __zStackWasm?: Bindings & { warmOrchardProvingKey?: () => boolean } };
    const warmed = !canUseProveWorker() && (g.__zStackWasm?.warmOrchardProvingKey?.() ?? false);
    return { ready: warmed, ms: Date.now() - start };
  } catch {
    return { ready: false, ms: Date.now() - start };
  }
}

export function orchardProvingKeyReady(): boolean {
  const scan = workerScanSession();
  if (scan?.warmProvingKey) return runtimeState.scanWorkerKeyFor === scan;
  const g = globalThis as unknown as { __zStackWasm?: Bindings & { orchardProvingKeyReady?: () => boolean } };
  return canUseProveWorker() ? runtimeState.workerProvingKeyReady : !!g.__zStackWasm?.orchardProvingKeyReady?.();
}
