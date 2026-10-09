/** Local engine startup only. No wallet identifiers, URLs or provider messages. */
export type EngineLoadProgress = {
  component: "keys" | "scanner";
  phase: "download" | "verify" | "initialize" | "ready" | "fallback";
  /** Decoded WASM bytes; compressed transfer sizes can differ. */
  loadedBytes?: number;
  /** Present only when the decoded size is known. */
  totalBytes?: number;
};

const listeners = new Set<(progress: EngineLoadProgress) => void>();

export function observeEngineProgress(handler: (progress: EngineLoadProgress) => void): () => void {
  listeners.add(handler);
  return () => { listeners.delete(handler); };
}

export function reportEngineProgress(progress: EngineLoadProgress): void {
  for (const handler of listeners) {
    try { handler({ ...progress }); } catch { console.warn("engine progress handler"); }
  }
}
