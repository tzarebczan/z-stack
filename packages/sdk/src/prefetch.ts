/**
 * Sliding-window compact-block fetch.
 *
 * `prefetch` is unresolved HTTP calls only. Finished blobs waiting for
 * sequential apply must not occupy a fetch slot — otherwise the first wave
 * completes, the queue looks "full", and the next GetBlockRange starts
 * one-at-a-time after each apply.
 */

export type PrefetchedRange = {
  start: number;
  end: number;
  blob: Uint8Array;
};

export type BlockPrefetch = {
  next(): Promise<PrefetchedRange | null>;
  inFlight(): number;
  buffered(): number;
  dispose(): void;
};

export type BlockPrefetchOpts = {
  start: number;
  tip: number;
  batch: number;
  prefetch: number;
  /** Total reserved ranges (in flight + buffered). Default 2× prefetch. */
  buffer?: number;
  fetch: (start: number, end: number, signal: AbortSignal) => Promise<Uint8Array>;
  signal?: AbortSignal;
  cancelled?: () => boolean;
};

export function createBlockPrefetch(opts: BlockPrefetchOpts): BlockPrefetch {
  for (const [name, value] of Object.entries({ start: opts.start, tip: opts.tip, batch: opts.batch, prefetch: opts.prefetch, buffer: opts.buffer ?? opts.prefetch * 2 })) {
    if (!Number.isSafeInteger(value) || value < (name === "start" || name === "tip" ? 0 : 1)) {
      throw new Error(`invalid prefetch ${name}: ${value}`);
    }
  }
  const prefetch = Math.max(1, Math.floor(opts.prefetch));
  const buffer = Math.max(prefetch, Math.floor(opts.buffer ?? prefetch * 2));
  const cancelled = opts.cancelled ?? (() => false);
  let nextFetch = opts.start;
  let nextApply = opts.start;
  let inFlight = 0;
  let live = true;
  let fail: unknown;
  let failed = false;
  const controller = new AbortController();
  const ready = new Map<number, PrefetchedRange>();
  const listeners = new Set<() => void>();

  const notify = () => {
    for (const l of [...listeners]) l();
  };

  const waitUntil = (pred: () => boolean): Promise<void> => {
    if (pred()) return Promise.resolve();
    return new Promise((resolve) => {
      const l = () => {
        if (!pred()) return;
        listeners.delete(l);
        resolve();
      };
      listeners.add(l);
      if (pred()) {
        listeners.delete(l);
        resolve();
      }
    });
  };

  const dispose = () => {
    if (!live) return;
    live = false;
    controller.abort();
    ready.clear();
    opts.signal?.removeEventListener("abort", dispose);
    notify();
    listeners.clear();
  };

  const launch = () => {
    if (cancelled() || opts.signal?.aborted) dispose();
    if (!live || failed) return;
    while (inFlight < prefetch && ready.size + inFlight < buffer && nextFetch <= opts.tip) {
      const start = nextFetch;
      const end = Math.min(opts.tip, start + opts.batch - 1);
      nextFetch = end + 1;
      inFlight += 1;
      void Promise.resolve().then(() => {
        controller.signal.throwIfAborted();
        return opts.fetch(start, end, controller.signal);
      }).then(
        (blob) => {
          inFlight -= 1;
          if (!live || failed) return;
          ready.set(start, { start, end, blob });
          launch();
          notify();
        },
        (e) => {
          inFlight -= 1;
          if (!live || failed) return;
          fail = e;
          failed = true;
          controller.abort();
          ready.clear();
          opts.signal?.removeEventListener("abort", dispose);
          notify();
        },
      );
    }
  };

  opts.signal?.addEventListener("abort", dispose, { once: true });
  launch();

  return {
    inFlight: () => inFlight,
    buffered: () => ready.size,
    dispose,
    next: async () => {
      for (;;) {
        if (!live) throw new Error("sync cancelled");
        if (cancelled()) {
          dispose();
          throw new Error("sync cancelled");
        }
        if (failed) throw fail;
        if (nextApply > opts.tip) return null;
        const job = ready.get(nextApply);
        if (job) {
          ready.delete(nextApply);
          nextApply = job.end + 1;
          launch();
          return job;
        }
        if (inFlight === 0 && nextFetch > opts.tip) {
          throw new Error(`prefetch stalled at ${nextApply} (tip ${opts.tip})`);
        }
        launch();
        await waitUntil(
          () => !live || cancelled() || failed || ready.has(nextApply) || (inFlight === 0 && nextFetch > opts.tip),
        );
      }
    },
  };
}
