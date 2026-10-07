/** App-owned, page-memory-only confirmation before durable creation. */
export function recoveryMemory() {
  let pending: Readonly<{ address: string; phrase: string }> | undefined;
  let finish: ((error?: unknown) => void) | undefined;
  const listeners = new Set<() => void>();
  const notify = () => {
    for (const listener of listeners) { try { listener(); } catch { /* UI owns its errors. */ } }
  };
  return {
    snapshot: () => pending,
    subscribe(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener); }; },
    prepare(address: string, phrase: string, signal: AbortSignal) {
      if (pending) return Promise.reject(new Error("Recovery confirmation is already pending."));
      return new Promise<void>((resolve, reject) => {
        const cancel = () => complete(new DOMException("Creation cancelled", "AbortError"));
        const complete = (error?: unknown) => {
          signal.removeEventListener("abort", cancel);
          pending = undefined; finish = undefined; notify();
          if (error) reject(error); else resolve();
        };
        pending = Object.freeze({ address, phrase }); finish = complete;
        signal.addEventListener("abort", cancel, { once: true });
        if (signal.aborted) cancel(); else notify();
      });
    },
    acknowledge(address: string) {
      if (pending?.address === address) finish?.();
    },
  };
}

export const pendingRecovery = recoveryMemory();
