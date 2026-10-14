import { abortable } from "./abort";

/** Public routing metadata and encrypted PIR messages only. No wallet identities or positions. */
export type PrivateEnhanceRequest =
  | { kind: "init" }
  | { kind: "session"; sessionId: string }
  | { kind: "query"; body: Uint8Array; anchor: { height: number; hash: string } };
export type PrivateEnhanceTransport = {
  exchange: (request: PrivateEnhanceRequest, signal: AbortSignal) => Promise<{
    status: 200 | 409 | 410 | 429 | 503;
    body: Uint8Array;
  }>;
};
export type PrivateEnhanceOperation = "pending" | "begin" | "material" | "prepare" | "finish" | "anchor" | "cancel";
export type PrivateEnhanceHost = {
  privateEnhance?: (operation: PrivateEnhanceOperation, bytes: Uint8Array, time: number) => Promise<Uint8Array>;
};
const EMPTY = new Uint8Array();
const text = new TextDecoder("utf-8", { fatal: true });

/** One bounded pass. Rust owns all query context and authenticates records before applying them. */
export async function syncPrivateEnhance({ source, transport, signal, assertCurrent, checkpoint }: {
  source: PrivateEnhanceHost;
  transport: PrivateEnhanceTransport;
  signal: AbortSignal;
  assertCurrent: () => void;
  checkpoint: () => Promise<void>;
}): Promise<number> {
  const engine = source.privateEnhance;
  if (!engine) throw new Error("WASM needs an Enhance v7 upgrade");
  const current = () => { signal.throwIfAborted(); assertCurrent(); };
  const invoke = async (operation: PrivateEnhanceOperation, bytes: Uint8Array = EMPTY) => {
    current();
    const result = await engine(operation, bytes, Math.floor(Date.now() / 1000));
    current();
    return result;
  };
  const exchange = async (request: PrivateEnhanceRequest, limit: number) => {
    current();
    const result = await abortable(signal, () => transport.exchange(request, signal));
    current();
    if (result.status === 409 || result.status === 410) throw new Error("enhance_refresh_required");
    if (result.status !== 200) throw new Error("enhance_unavailable");
    if (!(result.body instanceof Uint8Array) || result.body.length > limit) throw new Error("enhance_size_limit");
    return result.body;
  };
  let applied = 0;
  try {
    const pending = await invoke("pending");
    if (pending.length !== 1 || pending[0] > 1) throw new Error("enhance_engine_invalid");
    if (pending[0] === 0) return 0;
    const manifest = await exchange({ kind: "init" }, 1024 * 1024);
    const sessions: unknown = JSON.parse(text.decode(await invoke("begin", manifest)));
    if (!Array.isArray(sessions) || sessions.length > 24 || sessions.some(id => typeof id !== "string" || !/^[0-9a-f]{64}$/.test(id))) {
      throw new Error("enhance_engine_invalid");
    }
    for (const sessionId of sessions) {
      const material = await exchange({ kind: "session", sessionId }, 256 * 1024);
      await invoke("material", material);
    }
    const anchor: unknown = JSON.parse(text.decode(await invoke("anchor")));
    if (typeof anchor !== "object" || anchor === null || !("height" in anchor) || !("hash" in anchor)
      || !Number.isSafeInteger(anchor.height) || typeof anchor.height !== "number" || anchor.height < 0
      || typeof anchor.hash !== "string" || !/^[0-9a-f]{64}$/.test(anchor.hash)) throw new Error("enhance_engine_invalid");
    for (let count = 0; count < 24; count++) {
      const body = await invoke("prepare");
      if (body.length === 0) break;
      if (body.length < 116 || body.length > 282740) throw new Error("enhance_engine_invalid");
      const reply = await exchange({ kind: "query", body, anchor: { height: anchor.height, hash: anchor.hash } }, 30836);
      const result = text.decode(await invoke("finish", reply));
      if (!["Stored", "AlreadyResolved", "NotRecoverable"].includes(result)) throw new Error("enhance_engine_invalid");
      if (result === "Stored") applied++;
      await checkpoint();
      current();
    }
    return applied;
  } finally {
    // Retire private entropy even after a provider failure, timeout, policy change or owner close.
    // An obsolete session's own RPC guard refuses without touching its successor.
    await engine("cancel", EMPTY, Math.floor(Date.now() / 1000)).catch(() => {});
  }
}
