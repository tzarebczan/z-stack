/// Compact-block scan worker — MT/ST wasm lives here so the UI thread stays free.

import { allowMissingBuiltWasm, verifyWasmAt } from "./integrity";
import { runHardware, type HardwareAction, type HardwareStatics, type HardwareWalletHandle } from "./hardware-ops";

type InitMod = {
  default: (args?: unknown) => Promise<unknown>;
  initThreadPool?: (n: number) => Promise<void>;
  threadCount?: () => number;
  warmOrchardProvingKey?: () => boolean;
  orchardProvingKeyReady?: () => boolean;
  setRegtestNu63Height?: (height: number) => void;
  setRegtestNu7Height?: (height: number) => void;
  WasmWallet: {
    create: (network: string, mnemonic: string, birthday: number, accountIndex: number) => Wallet;
    fromSnapshot: (bytes: Uint8Array) => Wallet;
    fromUfvk?: (network: string, ufvk: string, birthday: number, accountIndex: number) => Wallet;
    fromHardware?: (
      network: string,
      ufvk: string,
      birthday: number,
      device: string,
      seedFingerprint: string,
      accountIndex: number,
    ) => Wallet;
    capabilities: () => string;
  } & HardwareStatics;
};

type Wallet = HardwareWalletHandle & {
  free?: () => void;
  applyCompactBlocksSummary: (blob: Uint8Array) => string;
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
  proveSend?: (mnemonic: string, to: string, amountZec: string, memo?: string) => string;
  proveShield?: (mnemonic: string, thresholdZat: number) => string;
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
  unifiedAddress: () => string;
};

type Req = {
  id: number;
  op: string;
  preferMulticore?: boolean;
  wasmBasePath?: string;
  threads?: number;
  network?: string;
  mnemonic?: string;
  ufvk?: string;
  birthday?: number;
  accountIndex?: number;
  snapshot?: ArrayBuffer;
  blob?: ArrayBuffer;
  transparent?: boolean;
  json?: string;
  protocol?: string;
  hex?: string;
  height?: number;
  server?: string;
  limit?: number;
  to?: string;
  amountZec?: string;
  maxFeeZat?: string;
  memo?: string;
  kind?: "send" | "sendTransparent" | "shield";
  thresholdZat?: number;
  regtestNu63Height?: number;
  regtestNu7Height?: number;
  device?: string;
  seedFingerprint?: string;
  action?: HardwareAction;
  pczt?: ArrayBuffer;
  signed?: ArrayBuffer;
  copy?: "full" | "compact" | "batch";
  appVersion?: string;
  txid?: string;
};

let wasm: InitMod | null = null;
let wallet: Wallet | null = null;
let mode: "single-thread" | "multi-thread" = "single-thread";

async function loadWasm(preferMulticore: boolean, threads: number, wasmBasePath?: string): Promise<InitMod> {
  const base = wasmBasePath?.replace(/\/+$/, "");
  if (preferMulticore && !base) {
    try {
      const mt = (await import("./generated-mt/z_wasm.js")) as unknown as InitMod;
      const wasmUrl = new URL("./generated-mt/z_wasm_bg.wasm", import.meta.url);
      const bytes = await verifyWasmAt(wasmUrl.href, new URL("./generated-mt/integrity.json", import.meta.url).href, {
        allowMissing: allowMissingBuiltWasm(false),
      });
      await mt.default({ module_or_path: bytes ?? wasmUrl });
      if (typeof mt.initThreadPool !== "function") throw new Error("multicore wasm is missing initThreadPool");
      await mt.initThreadPool(Math.max(1, threads));
      mode = "multi-thread";
      return mt;
    } catch {
      console.warn("scan worker MT failed; using single-thread");
    }
  }
  const st = (await import("./generated/z_wasm.js")) as unknown as InitMod;
  const wasmUrl = base ? `${base}/z_wasm_bg.wasm` : new URL("./generated/z_wasm_bg.wasm", import.meta.url).href;
  const integrityUrl = base ? `${base}/integrity.json` : new URL("./generated/integrity.json", import.meta.url).href;
  const bytes = await verifyWasmAt(wasmUrl, integrityUrl, { allowMissing: allowMissingBuiltWasm(!!base) });
  await st.default({ module_or_path: bytes ?? wasmUrl });
  mode = "single-thread";
  return st;
}

function replaceWallet(next: Wallet | null): void {
  wallet?.free?.();
  wallet = next;
}

function needWallet(): Wallet {
  if (!wallet) throw new Error("no wasm wallet in scan worker");
  return wallet;
}

function needWasm(): InitMod {
  if (!wasm) throw new Error("scan worker not initialized");
  return wasm;
}

function applyBlob(buf: ArrayBuffer, transparent = false): { notesFound: number; spendsFound: number; scanned: number } {
  const w = needWallet();
  if (transparent && !w.applyTransparentBlocks) throw new Error("WASM needs a public-data upgrade");
  const blob = new Uint8Array(buf);
  let notesFound = 0;
  let spendsFound = 0;
  if (typeof w.applyCompactBlocksSummary === "function") {
    const s = JSON.parse(w.applyCompactBlocksSummary(blob)) as {
      notesFound?: number;
      spendsFound?: number;
    };
    notesFound = s.notesFound ?? 0;
    spendsFound = s.spendsFound ?? 0;
  } else {
    const deltas = JSON.parse(w.applyCompactBlocks(blob)) as Array<{
      notesFound?: number;
      spendsFound?: number;
    }>;
    for (const d of deltas) {
      notesFound += d.notesFound ?? 0;
      spendsFound += d.spendsFound ?? 0;
    }
  }
  if (transparent) {
    if (!w.applyTransparentBlocks) throw new Error("WASM needs a public-data upgrade");
    w.applyTransparentBlocks(blob);
  }
  return { notesFound, spendsFound, scanned: w.scannedHeight() };
}

async function handle(msg: Req): Promise<void> {
  const post = self.postMessage.bind(self) as (data: unknown, transfer?: Transferable[]) => void;
  try {
    switch (msg.op) {
      case "init": {
        wasm = await loadWasm(!!msg.preferMulticore, msg.threads ?? 8, msg.wasmBasePath);
        if (msg.regtestNu63Height) wasm.setRegtestNu63Height?.(msg.regtestNu63Height);
        if (msg.regtestNu7Height) wasm.setRegtestNu7Height?.(msg.regtestNu7Height);
        const caps = JSON.parse(needWasm().WasmWallet.capabilities()) as {
          simd?: boolean;
          orchardCircuit?: boolean;
          threads?: number;
          multicore?: boolean;
        };
        post({
          id: msg.id,
          mode,
          threads: needWasm().threadCount?.() ?? caps.threads ?? msg.threads ?? 1,
          simd: !!caps.simd,
          orchardCircuit: !!caps.orchardCircuit,
          multicore: mode === "multi-thread",
          sharedArrayBuffer: typeof SharedArrayBuffer !== "undefined",
          crossOriginIsolated: typeof crossOriginIsolated !== "undefined" && crossOriginIsolated,
        });
        return;
      }
      case "create": {
        replaceWallet(needWasm().WasmWallet.create(
          msg.network ?? "testnet",
          msg.mnemonic ?? "",
          msg.birthday ?? 1,
          msg.accountIndex ?? 0,
        ));
        post({ id: msg.id, ok: true });
        return;
      }
      case "fromUfvk": {
        const fromUfvk = needWasm().WasmWallet.fromUfvk;
        if (typeof fromUfvk !== "function") throw new Error("this wasm build cannot restore from UFVK");
        replaceWallet(fromUfvk(msg.network ?? "testnet", msg.ufvk ?? "", msg.birthday ?? 1, msg.accountIndex ?? 0));
        post({ id: msg.id, ok: true });
        return;
      }
      case "fromHardware": {
        const fromHardware = needWasm().WasmWallet.fromHardware;
        if (typeof fromHardware !== "function") throw new Error("this wasm build has no hardware-wallet support");
        replaceWallet(fromHardware(
          msg.network ?? "testnet",
          msg.ufvk ?? "",
          msg.birthday ?? 1,
          msg.device ?? "",
          msg.seedFingerprint ?? "",
          msg.accountIndex ?? 0,
        ));
        post({ id: msg.id, ok: true });
        return;
      }
      case "hardware": {
        const out = runHardware(needWallet(), needWasm().WasmWallet, msg.action as HardwareAction, {
          ...(msg.pczt ? { pczt: new Uint8Array(msg.pczt) } : {}),
          ...(msg.signed ? { signed: new Uint8Array(msg.signed) } : {}),
          ...(msg.to !== undefined ? { to: msg.to } : {}),
          ...(msg.amountZec !== undefined ? { amountZec: msg.amountZec } : {}),
          ...(msg.memo !== undefined ? { memo: msg.memo } : {}),
          ...(msg.copy ? { copy: msg.copy } : {}),
          ...(msg.json !== undefined ? { json: msg.json } : {}),
          ...(msg.appVersion !== undefined ? { appVersion: msg.appVersion } : {}),
          ...(msg.txid !== undefined ? { txid: msg.txid } : {}),
        });
        const buf = out.bytes?.buffer as ArrayBuffer | undefined;
        post({ id: msg.id, bytes: buf, json: out.json, n: out.n }, buf ? [buf] : []);
        return;
      }
      case "fromSnapshot": {
        if (!msg.snapshot) throw new Error("fromSnapshot needs bytes");
        replaceWallet(needWasm().WasmWallet.fromSnapshot(new Uint8Array(msg.snapshot)));
        post({ id: msg.id, ok: true });
        return;
      }
      case "applyBlob": {
        if (!msg.blob) throw new Error("applyBlob needs bytes");
        post({ id: msg.id, ...applyBlob(msg.blob, msg.transparent) });
        return;
      }
      case "applyTreeState": {
        const w = needWallet();
        if (typeof w.applyTreeState !== "function") throw new Error("no applyTreeState");
        w.applyTreeState(msg.json ?? "{}");
        post({ id: msg.id, ok: true, treesReady: !!w.treesReady?.() });
        return;
      }
      case "applySubtreeRoots": {
        const w = needWallet();
        const n = w.applySubtreeRoots?.(msg.protocol ?? "orchard", msg.json ?? "{}") ?? 0;
        post({ id: msg.id, n });
        return;
      }
      case "applyUtxos": {
        const n = needWallet().applyUtxos?.(msg.json ?? "{}") ?? 0;
        post({ id: msg.id, n });
        return;
      }
      case "applyMempool": {
        const n = needWallet().applyMempool?.(msg.json ?? "{}") ?? 0;
        post({ id: msg.id, n });
        return;
      }
      case "applyTransparentBlocks": {
        const w = needWallet();
        if (!w.applyTransparentBlocks || !msg.blob) throw new Error("WASM needs a public-data upgrade");
        post({ id: msg.id, n: w.applyTransparentBlocks(new Uint8Array(msg.blob)) });
        return;
      }
      case "applySharedMemos": {
        const w = needWallet();
        if (!w.applySharedMemos) throw new Error("WASM needs a public-data upgrade");
        post({ id: msg.id, n: w.applySharedMemos(msg.json ?? "") });
        return;
      }
      case "enhanceRawTx": {
        const n = needWallet().enhanceRawTx?.(msg.hex ?? "") ?? 0;
        post({ id: msg.id, n });
        return;
      }
      case "rewindTo": {
        const h = needWallet().rewindTo?.(msg.height ?? 0) ?? 0;
        post({ id: msg.id, height: h });
        return;
      }
      case "rescanFrom": {
        const w = needWallet();
        if (!w.rescanFrom) throw new Error("this wasm build cannot rescan an earlier birthday");
        if (!Number.isInteger(msg.birthday) || msg.birthday! < 1 || msg.birthday! > 0xffff_ffff) throw new Error("invalid birthday height");
        w.rescanFrom(msg.birthday!);
        post({ id: msg.id });
        break;
      }
      case "resetScan": {
        const w = needWallet();
        if (typeof w.resetScan !== "function") throw new Error("this wasm build cannot reset scan");
        w.resetScan();
        post({ id: msg.id, ok: true });
        return;
      }
      case "recomputePools": {
        const w = needWallet();
        if (typeof w.recomputePoolsWithTick === "function") {
          w.recomputePoolsWithTick((hashed, total, message) => {
            post({ id: msg.id, progress: true, hashed, total, message });
          });
        } else {
          w.recomputePools?.();
        }
        post({ id: msg.id, ok: true });
        return;
      }
      case "nextUnifiedAddress": {
        needWallet().nextUnifiedAddress();
        post({ id: msg.id, ok: true });
        return;
      }
      case "attachSeed": {
        const w = needWallet();
        if (typeof w.attachSeed !== "function") throw new Error("attachSeed missing");
        w.attachSeed(msg.mnemonic ?? "");
        post({ id: msg.id, ok: true });
        return;
      }
      case "estimateFee": {
        const w = needWallet();
        if (typeof w.estimateFee !== "function") throw new Error("this wasm build cannot estimate fees");
        post({ id: msg.id, json: w.estimateFee(msg.to ?? "", msg.amountZec ?? "", msg.memo) });
        return;
      }
      case "supportsTransparentSend": {
        const w = needWallet();
        post({ id: msg.id, supported: typeof w.estimateTransparentFee === "function" && typeof w.proveTransparentSend === "function" });
        return;
      }
      case "estimateTransparentFee": {
        const w = needWallet();
        if (typeof w.estimateTransparentFee !== "function") throw new Error("this wasm build cannot estimate transparent swap outputs");
        post({ id: msg.id, json: w.estimateTransparentFee(msg.to ?? "", msg.amountZec ?? "") });
        return;
      }
      case "maxSend": {
        const w = needWallet();
        if (typeof w.maxSend !== "function") throw new Error("this wasm build cannot estimate max send");
        post({ id: msg.id, json: w.maxSend(msg.to) });
        return;
      }
      case "prove": {
        // Proves on this worker's wallet: its trees are already built and, on
        // the multicore build, the proof runs on the Rayon pool. The wallet
        // records the pending transaction, as a prove-worker round trip did.
        const w = needWallet();
        const out = msg.kind === "shield"
          ? w.proveShield?.(msg.mnemonic ?? "", msg.thresholdZat ?? 100_000)
          : msg.kind === "sendTransparent"
            ? w.proveTransparentSend?.(msg.mnemonic ?? "", msg.to ?? "", msg.amountZec ?? "0", msg.maxFeeZat)
          : w.proveSend?.(msg.mnemonic ?? "", msg.to ?? "", msg.amountZec ?? "0", msg.memo);
        if (typeof out !== "string") throw new Error("this wasm build cannot prove");
        const { hex, txid } = JSON.parse(out) as { hex?: string; txid?: string };
        if (!hex) throw new Error("prove returned no hex");
        post({ id: msg.id, hex, txid });
        return;
      }
      case "warmProvingKey": {
        const start = Date.now();
        const ready = needWasm().warmOrchardProvingKey?.() ?? false;
        post({ id: msg.id, ready, ms: Date.now() - start });
        return;
      }
      case "snapshotJson": {
        post({ id: msg.id, json: needWallet().snapshotJson(msg.server ?? "") });
        return;
      }
      case "pendingRawTxs": {
        post({ id: msg.id, json: needWallet().pendingRawTxs?.() ?? "[]" });
        return;
      }
      case "toSnapshot": {
        const snapshot = needWallet().toSnapshot();
        post({ id: msg.id, snapshot: snapshot.buffer }, [snapshot.buffer]);
        return;
      }
      case "persistenceSnapshot": {
        const w = needWallet();
        // Capture both synchronously; a queued apply cannot race the preview.
        const snapshot = w.toSnapshot();
        const json = w.snapshotJson("");
        post({ id: msg.id, snapshot: snapshot.buffer, json }, [snapshot.buffer]);
        return;
      }
      case "memoEnhancementTxids": {
        post({ id: msg.id, json: needWallet().memoEnhancementTxids?.(msg.limit ?? 40) ?? null });
        return;
      }
      case "history": {
        post({ id: msg.id, json: needWallet().history(msg.limit ?? 50) });
        return;
      }
      case "meta": {
        const w = needWallet();
        post({
          id: msg.id,
          scanned: w.scannedHeight(),
          birthday: w.birthday(),
          nextHeight: w.nextHeight(),
          transparentCompact: typeof w.applyTransparentBlocks === "function",
          treesReady: !!w.treesReady?.(),
          sinsemillaLive: !!w.sinsemillaLive?.(),
          hasIncrementalRoots: typeof w.subtreeRootCount === "function",
          saplingRoots: w.subtreeRootCount?.("sapling") ?? 0,
          orchardRoots: w.subtreeRootCount?.("orchard") ?? 0,
          ironwoodRoots: w.subtreeRootCount?.("ironwood") ?? 0,
          hasRootsStart: typeof w.subtreeRootsStart === "function",
          saplingRootsStart: w.subtreeRootsStart?.("sapling") ?? 0,
          orchardRootsStart: w.subtreeRootsStart?.("orchard") ?? 0,
          ironwoodRootsStart: w.subtreeRootsStart?.("ironwood") ?? 0,
          transparentAddress: w.transparentAddress(),
          unifiedAddress: w.unifiedAddress(),
        });
        return;
      }
      case "forget": {
        replaceWallet(null);
        post({ id: msg.id, ok: true });
        return;
      }
      default:
        throw new Error(`unknown scan op ${msg.op}`);
    }
  } catch (e) {
    post({
      id: msg.id,
      error: e instanceof Error ? e.message : String(e),
    });
  }
}

let handleChain: Promise<void> = Promise.resolve();
self.onmessage = (ev: MessageEvent<Req>) => {
  const msg = ev.data;
  handleChain = handleChain.then(() => handle(msg)).catch((e) => {
    (self.postMessage as typeof self.postMessage)({
      id: msg.id,
      error: e instanceof Error ? e.message : String(e),
    });
  });
};
