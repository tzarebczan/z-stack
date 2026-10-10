import { WalletError } from "@z-stack/core";
import { isTransientLightServerError } from "./lwd";

export const LIGHT_SERVER_GRACE_MS = 90_000;

export class LightServerUnavailableError extends WalletError {
  constructor(message: string, cause?: unknown) {
    super("transport", message, cause);
    this.name = "LightServerUnavailableError";
  }
}

export type LightServerRecovery = ReturnType<typeof createLightServerRecovery>;

/** One outage window across tip, frontier, and compact-block reads. */
export function createLightServerRecovery(opts: {
  signal: AbortSignal;
  cancelled?: () => boolean;
  graceMs?: number;
  retryBaseMs?: number;
  now?: () => number;
  onWaiting?: (remainingMs: number) => void;
}) {
  const graceMs = opts.graceMs ?? LIGHT_SERVER_GRACE_MS;
  const retryBaseMs = opts.retryBaseMs ?? 400;
  const now = opts.now ?? Date.now;
  let firstFailureAt: number | null = null;
  let attempts = 0;

  const checkCancelled = () => {
    if (opts.signal.aborted || opts.cancelled?.()) throw new Error("sync cancelled");
  };

  const reset = () => { firstFailureAt = null; attempts = 0; };

  const waitAfter = async (error: unknown): Promise<void> => {
    checkCancelled();
    if (!isTransientLightServerError(error)) throw error;
    const failedAt = now();
    firstFailureAt ??= failedAt;
    const remainingMs = graceMs - (failedAt - firstFailureAt);
    if (remainingMs <= 0) {
      throw new LightServerUnavailableError(
        `Light server has been unavailable for ${Math.ceil(graceMs / 1000)} seconds. Sync can resume when it returns.`,
        error,
      );
    }
    opts.onWaiting?.(remainingMs);
    // A single scan owns the retry clock. Increasing delay prevents eight
    // prefetched GETs from hammering a restarting local indexer.
    const delayMs = Math.min(remainingMs, retryBaseMs * 2 ** Math.min(attempts++, 3));
    await new Promise<void>((resolve, reject) => {
      checkCancelled();
      const cleanup = () => { clearTimeout(timer); opts.signal.removeEventListener("abort", onAbort); };
      const onAbort = () => { cleanup(); reject(new Error("sync cancelled")); };
      const timer = setTimeout(() => { cleanup(); resolve(); }, delayMs);
      opts.signal.addEventListener("abort", onAbort, { once: true });
    });
    checkCancelled();
  };

  return {
    reset,
    waitAfter,
    run: async <T>(work: () => Promise<T>): Promise<T> => {
      for (;;) {
        checkCancelled();
        try {
          const value = await work();
          checkCancelled();
          reset();
          return value;
        } catch (error) {
          await waitAfter(error);
        }
      }
    },
  };
}
