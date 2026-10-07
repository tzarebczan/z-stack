import type { Wallet } from "@z-stack/sdk";
/** Replaced by the scaffold only when --with-base is explicitly selected. */
export function attachBase(
  _wallet: Wallet,
  _run: <T>(action: () => Promise<T>) => Promise<T>,
  _identity: () => string | undefined,
): () => void {
  return () => {};
}
