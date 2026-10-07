/** React cleanup is synchronous; the next mount must await the prior close. */
export function walletLifetime<T extends { close(): Promise<void> }>(open: () => Promise<T>) {
  let previous: Promise<void> = Promise.resolve();
  return () => {
    let disposed = false;
    let release!: () => void;
    const released = new Promise<void>(resolve => { release = resolve; });
    const start = () => disposed ? undefined : open();
    const ready = previous.then(start, start);
    previous = ready.then(async owner => {
      await released;
      if (owner) await owner.close();
    }, async () => { await released; });
    // Unmounted UI cannot render a close error. Observe it; the next open still
    // passes through the SDK owner guard and loads durable state before actions.
    void previous.catch(() => {});
    return { ready, release() { disposed = true; release(); } };
  };
}
