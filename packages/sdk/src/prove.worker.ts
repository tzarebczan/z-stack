/// ST wasm prove worker — keeps the UI thread free during orchard proving / keygen.

import { allowMissingBuiltWasm, verifyWasmAt } from "./integrity";

import init, * as glue from "./generated/z_wasm.js";
import {
  WasmWallet,
  warmOrchardProvingKey,
  orchardProvingKeyReady,
} from "./generated/z_wasm.js";

type Req = {
  id: number;
  kind?: "init" | "send" | "sendTransparent" | "shield" | "warm" | "ready" | "hardwareProve";
  pczt?: ArrayBuffer;
  wasmBasePath?: string;
  regtestNu63Height?: number;
  snapshot?: ArrayBuffer;
  mnemonic?: string;
  to?: string;
  amountZec?: string;
  maxFeeZat?: string;
  memo?: string;
  thresholdZat?: number;
};

let ready = false;
let loadedBase: string | undefined;

async function ensure(wasmBasePath?: string, regtestNu63Height?: number): Promise<void> {
  const base = wasmBasePath?.replace(/\/+$/, "");
  if (ready) {
    if (base !== loadedBase) throw new Error("prove worker already initialized with another wasmBasePath");
    return;
  }
  const wasmUrl = base ? `${base}/z_wasm_bg.wasm` : new URL("./generated/z_wasm_bg.wasm", import.meta.url).href;
  const integrityUrl = base ? `${base}/integrity.json` : new URL("./generated/integrity.json", import.meta.url).href;
  const bytes = await verifyWasmAt(wasmUrl, integrityUrl, { allowMissing: allowMissingBuiltWasm(!!base) });
  await init({ module_or_path: bytes ?? wasmUrl });
  if (regtestNu63Height) {
    (glue as { setRegtestNu63Height?: (height: number) => void }).setRegtestNu63Height?.(regtestNu63Height);
  }
  loadedBase = base;
  ready = true;
}

async function handle(msg: Req): Promise<void> {
  try {
    await ensure(msg.wasmBasePath, msg.regtestNu63Height);
    if (msg.kind === "init" || msg.kind === "ready") {
      self.postMessage({ id: msg.id, ready: orchardProvingKeyReady() });
      return;
    }
    if (msg.kind === "warm") {
      const start = Date.now();
      const warmed = warmOrchardProvingKey();
      self.postMessage({ id: msg.id, ready: warmed, ms: Date.now() - start });
      return;
    }
    if (msg.kind === "hardwareProve") {
      // A hardware PCZT proves on its own; no wallet needed.
      const prove = (WasmWallet as unknown as { hardwareProve?: (p: Uint8Array) => Uint8Array }).hardwareProve;
      if (typeof prove !== "function" || !msg.pczt) throw new Error("this wasm build cannot prove hardware PCZTs");
      const proved = prove(new Uint8Array(msg.pczt));
      const post = self.postMessage.bind(self) as (msg: unknown, transfer?: Transferable[]) => void;
      post({ id: msg.id, pczt: proved.buffer }, [proved.buffer]);
      return;
    }
    if (!msg.snapshot) throw new Error("prove worker needs a wallet snapshot");
    const w = WasmWallet.fromSnapshot(new Uint8Array(msg.snapshot));
    try {
      const out: { hex?: string; txid?: string } = JSON.parse(
        msg.kind === "send"
          ? w.proveSend(msg.mnemonic ?? "", msg.to ?? "", msg.amountZec ?? "0", msg.memo)
          : msg.kind === "sendTransparent"
            ? w.proveTransparentSend(msg.mnemonic ?? "", msg.to ?? "", msg.amountZec ?? "0", msg.maxFeeZat)
          : w.proveShield(msg.mnemonic ?? "", msg.thresholdZat ?? 100_000),
      );
      if (!out.hex) throw new Error("prove returned no hex");
      const snapshot = w.toSnapshot();
      const post = self.postMessage.bind(self) as (msg: unknown, transfer?: Transferable[]) => void;
      post({ id: msg.id, hex: out.hex, txid: out.txid, snapshot }, [snapshot.buffer]);
    } finally {
      w.free();
    }
  } catch (e) {
    self.postMessage({
      id: msg.id,
      error: e instanceof Error ? e.message : String(e),
    });
  }
}

let jobs: Promise<void> = Promise.resolve();
self.onmessage = (ev: MessageEvent<Req>) => {
  jobs = jobs.then(() => handle(ev.data));
};
