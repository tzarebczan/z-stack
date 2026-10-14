import { WalletError } from "@z-stack/core";
import { abortable } from "./abort";

export type TransparentScanMode = "off" | "compact" | "pir";
export function transparentScanMode(value: unknown): TransparentScanMode {
  if (value === undefined) return "off";
  if (value === "off" || value === "compact" || value === "pir") return value;
  throw new WalletError("unknown", "transparentScan must be off, compact or pir.");
}

/** Rust-produced public routing selectors and PIR ciphertext. No scripts or wallet identifiers. */
export type PrivateTransparentTransport = {
  readonly sourceId: string;
  exchange: (request: {
    operation: Uint8Array;
    anchor: { height: number; hash: string };
  }, signal: AbortSignal) => Promise<Uint8Array>;
};
export type PrivateTransparentOperation = "begin" | "advance" | "accept" | "chain" | "ack" | "cancel" | "status";
export type PrivateTransparentHost = {
  privateTransparent?: (operation: PrivateTransparentOperation, bytes: Uint8Array) => Promise<Uint8Array>;
};

const EMPTY = new Uint8Array();
const decoder = new TextDecoder("utf-8", { fatal: true });
const encoder = new TextEncoder();
const encode = (value: unknown) => encoder.encode(JSON.stringify(value));
function point(value: unknown): { height: number; hash: string } {
  if (typeof value !== "object" || value === null || !("height" in value) || !("hash" in value)
    || typeof value.height !== "number" || !Number.isSafeInteger(value.height) || value.height < 0 || value.height > 0xffff_ffff
    || typeof value.hash !== "string" || !/^[0-9a-f]{64}$/.test(value.hash)) throw new Error("transparent_pir_engine_invalid");
  return { height: value.height, hash: value.hash };
}
function json(bytes: Uint8Array, max: number): unknown {
  if (!(bytes instanceof Uint8Array) || !bytes.length || bytes.length > max) throw new Error("transparent_pir_size_limit");
  return JSON.parse(decoder.decode(bytes));
}

/** One bounded Rust-owned recovery pass. A checkpoint is acknowledged only after storage commits. */
async function syncPrivateTransparentPass({ source, transport, signal, assertCurrent, checkpoint, treeState }: {
  source: PrivateTransparentHost;
  transport: PrivateTransparentTransport;
  signal: AbortSignal;
  assertCurrent: () => void;
  checkpoint: () => Promise<void>;
  treeState: ((height: number, signal: AbortSignal) => Promise<{ height: number; hash: string }>) | undefined;
}): Promise<"complete" | "scanning" | "scope-growth"> {
  const engine = source.privateTransparent;
  if (!engine) throw new Error("WASM needs a transparent PIR upgrade");
  if (!/^[0-9a-f]{64}$/.test(transport.sourceId)) throw new Error("transparent_pir_source_invalid");
  const current = () => { signal.throwIfAborted(); assertCurrent(); };
  const invoke = async (operation: PrivateTransparentOperation, bytes = EMPTY) => {
    current();
    const result = await engine(operation, bytes);
    current();
    return result;
  };
  try {
    const anchor = point(json(await invoke("begin", encode({ source_id: transport.sourceId })), 1024));
    await checkpoint();
    current();
    for (let steps = 0; steps < 512; steps++) {
      const step = json(await invoke("advance"), 1024 * 1024);
      if (typeof step !== "object" || step === null || !("kind" in step)) throw new Error("transparent_pir_engine_invalid");
      switch (step.kind) {
        case "exchange": {
          if (!("request_id" in step) || typeof step.request_id !== "number" || !Number.isSafeInteger(step.request_id)
            || step.request_id < 0 || !("operation" in step)) throw new Error("transparent_pir_engine_invalid");
          const result = await abortable(signal, () => transport.exchange({ operation: encode(step.operation), anchor }, signal));
          current();
          const reply = json(result, 24 * 1024 * 1024);
          await invoke("accept", encode({ request_id: step.request_id, reply }));
          break;
        }
        case "need_chain": {
          const required = point(step);
          if (!treeState) throw new Error("transparent_pir_chain_unavailable");
          const accepted = point(await abortable(signal, () => treeState(required.height, signal)));
          current();
          if (accepted.height !== required.height) throw new Error("transparent_pir_chain_invalid");
          // Rust compares this independently retrieved point with the advertised shard anchor.
          await invoke("chain", encode(accepted));
          break;
        }
        case "checkpoint": {
          if (!("checkpoint_id" in step) || typeof step.checkpoint_id !== "string"
            || !/^[0-9a-f]{64}$/.test(step.checkpoint_id)) throw new Error("transparent_pir_engine_invalid");
          await checkpoint();
          current();
          await invoke("ack", encoder.encode(step.checkpoint_id));
          await checkpoint();
          current();
          break;
        }
        case "stopped": {
          if (!("status" in step) || typeof step.status !== "object" || step.status === null || !("kind" in step.status))
            throw new Error("transparent_pir_engine_invalid");
          if (step.status.kind !== "complete") return "scanning";
          const status = json(await invoke("status"), 16 * 1024);
          return typeof status === "object" && status !== null && "completion" in status
            && status.completion === "complete-for-enrolled-scope" ? "complete" : "scope-growth";
        }
        default: throw new Error("transparent_pir_engine_invalid");
      }
    }
    return "scanning";
  } finally {
    // Cancel only this engine session; obsolete worker RPC guards cannot reach a successor.
    await engine("cancel", EMPTY).catch(() => {});
  }
}

/** Repeat activity-driven gap expansion within one bounded, cancellable sync task. */
export async function syncPrivateTransparent(input: Parameters<typeof syncPrivateTransparentPass>[0]): Promise<"complete" | "scanning"> {
  const signal = AbortSignal.any([input.signal, AbortSignal.timeout(120_000)]);
  let exchanges = 0, chainReads = 0, downloaded = 0, uploaded = 0;
  const budgetExceeded = new Error("transparent_pir_pass_budget");
  const transport: PrivateTransparentTransport = { sourceId: input.transport.sourceId, exchange: async (request, wait) => {
    if (++exchanges > 64 || uploaded + request.operation.length > 32 * 1024 * 1024) throw budgetExceeded;
    uploaded += request.operation.length;
    const reply = await input.transport.exchange(request, wait);
    downloaded += reply.length;
    if (downloaded > 256 * 1024 * 1024) throw budgetExceeded;
    return reply;
  } };
  const readChain = input.treeState;
  const treeState = readChain ? async (height: number, wait: AbortSignal) => {
    if (++chainReads > 64) throw budgetExceeded;
    return readChain(height, wait);
  } : undefined;
  try {
    for (let pass = 0; pass < 4; pass++) {
      const result = await syncPrivateTransparentPass({ ...input, transport, signal, treeState });
      if (result !== "scope-growth") return result;
    }
    return "scanning";
  } catch (error) {
    if (error === budgetExceeded) return "scanning";
    throw error;
  }
}
