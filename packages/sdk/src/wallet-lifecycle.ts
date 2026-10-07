/** Ownership of a wallet operation, independent of ordinary sync cancellation. */
export type WalletOperation = {
  readonly signal: AbortSignal;
  assertCurrent(): void;
  assertReady(): void;
  ready(): Promise<void>;
};

let owner = new AbortController();
let forgetting: Promise<void> | null = null;

export function captureWalletOperation(): WalletOperation {
  const signal = owner.signal;
  const barrier = forgetting;
  const assertCurrent = () => signal.throwIfAborted();
  return {
    signal,
    assertCurrent,
    assertReady() {
      assertCurrent();
      if (forgetting) throw new DOMException("wallet forget is still in progress; retry afterward", "AbortError");
    },
    async ready() {
      if (barrier) await barrier;
      assertCurrent();
    },
  };
}

/** Latest explicit create/restore/reset owns the wallet, from its first await. */
export function beginWalletOperation(): WalletOperation {
  owner.abort(new DOMException("wallet operation cancelled by replacement or forget", "AbortError"));
  owner = new AbortController();
  return captureWalletOperation();
}

/** Cancel only the caller's lease; a later replacement must not be cancelled with it. */
export function cancelWalletOperation(operation: WalletOperation, reason?: unknown): boolean {
  if (operation.signal !== owner.signal || operation.signal.aborted) return false;
  owner.abort(reason ?? new DOMException("wallet operation cancelled", "AbortError"));
  // Loading the untouched saved wallet must remain possible after cancellation.
  owner = new AbortController();
  return true;
}

/** Publish the cleanup barrier synchronously; new operations cannot race deletion. */
export function runWalletForget(cleanup: () => Promise<void>): Promise<void> {
  beginWalletOperation();
  const previous = forgetting;
  const work = (async () => {
    if (previous) await previous.catch(() => {});
    await cleanup();
  })();
  forgetting = work;
  const release = () => { if (forgetting === work) forgetting = null; };
  void work.then(release, release);
  return work;
}
