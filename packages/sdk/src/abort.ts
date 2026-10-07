/** Cancellation helpers with no wallet, storage or engine dependencies. */
export function aborted(message = "wallet operation cancelled"): DOMException {
  return new DOMException(message, "AbortError");
}

export function checkSignal(signal?: AbortSignal): void {
  if (signal?.aborted) throw signal.reason ?? aborted();
}

/** WebCrypto is not interruptible; discard its late result and reject promptly. */
export function abortable<T>(signal: AbortSignal | undefined, start: () => Promise<T>): Promise<T> {
  checkSignal(signal);
  return new Promise<T>((resolve, reject) => {
    const cancel = () => { cleanup(); reject(signal?.reason ?? aborted()); };
    const cleanup = () => signal?.removeEventListener("abort", cancel);
    signal?.addEventListener("abort", cancel, { once: true });
    try {
      start().then(value => {
        cleanup();
        if (signal?.aborted) reject(signal.reason ?? aborted());
        else resolve(value);
      }, error => { cleanup(); reject(error); });
    } catch (error) { cleanup(); reject(error); }
  });
}
