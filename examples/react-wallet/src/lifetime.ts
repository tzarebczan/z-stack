/** App-owned lifetime queue. React cleanup cannot await close; the next mount can. */
export function walletLifetime<T extends { close(): Promise<void> }>(open: () => Promise<T>) {
  let previous: Promise<void> = Promise.resolve();
  return () => {
    let disposed = false;
    let resolveRelease!: () => void;
    const released = new Promise<void>(resolve => { resolveRelease = resolve; });
    // close can finish cleanup while reporting a failed rollback commit. The
    // next open checks SDK ownership; a genuinely unreleased owner stays busy.
    const start = () => disposed ? undefined : open();
    const ready = previous.then(start, start);
    previous = ready.then(async owner => {
      await released;
      if (owner) await owner.close();
    }, async () => { await released; });
    // React cleanup cannot await this; observe its rejection immediately.
    void previous.catch(() => {});
    return { ready, release() { disposed = true; resolveRelease(); } };
  };
}
