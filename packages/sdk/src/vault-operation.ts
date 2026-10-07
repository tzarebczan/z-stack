import { captureWalletOperation } from "./wallet-lifecycle";
import { aborted, abortable, checkSignal, readVaultGeneration, type VaultDomain, type WalletGeneration } from "./wallet-storage";

const pending: Record<VaultDomain, Set<AbortController>> = { seed: new Set(), passkey: new Set() };

/** Standalone deletion invalidates only its vault. Wallet forget invalidates both. */
export function invalidateVaultOperations(domain?: VaultDomain): void {
  for (const key of domain ? [domain] : ["seed", "passkey"] as const) {
    for (const operation of pending[key]) operation.abort(aborted());
  }
}

export function beginVaultOperation(
  domain: VaultDomain, caller?: AbortSignal, webauthn = false,
  expectedGeneration?: WalletGeneration | Promise<WalletGeneration>,
) {
  // A stale enclosing create/restore must not capture a newer wallet identity.
  checkSignal(caller);
  const lifecycle = captureWalletOperation();
  lifecycle.assertCurrent();
  if (webauthn) lifecycle.assertReady();
  const controller = new AbortController();
  const sources = [caller, lifecycle.signal].filter((s): s is AbortSignal => !!s);
  const listeners = sources.map(source => {
    const cancel = () => controller.abort(source.reason ?? aborted());
    source.addEventListener("abort", cancel, { once: true });
    if (source.aborted) cancel();
    return () => source.removeEventListener("abort", cancel);
  });
  pending[domain].add(controller);
  const assertCurrent = () => { checkSignal(controller.signal); lifecycle.assertCurrent(); };
  const ready = async () => {
    await abortable(controller.signal, () => lifecycle.ready());
    assertCurrent();
  };
  // Dispatch now, before PBKDF/WebAuthn. Writes compare this exact identity.
  const generation = ready().then(async () => {
    const captured = await readVaultGeneration(domain, controller.signal);
    if (expectedGeneration !== undefined) {
      const expected = await abortable(controller.signal, () => Promise.resolve(expectedGeneration));
      if (expected !== captured.wallet) throw aborted("saved wallet changed; retry the operation");
    }
    assertCurrent();
    return captured;
  });
  // Failed identity reads also stop a pending chooser or expensive crypto step.
  void generation.catch(error => controller.abort(error));
  return {
    signal: controller.signal, generation, assertCurrent, ready,
    wait: <T>(start: () => Promise<T>) => abortable(controller.signal, start),
    dispose: () => {
      pending[domain].delete(controller);
      listeners.forEach(remove => remove());
      // Validation/chooser errors can finish before their background identity read.
      controller.abort(aborted("vault operation finished"));
    },
  };
}
