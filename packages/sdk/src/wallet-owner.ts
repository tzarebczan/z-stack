/** Realm-local ownership metadata. No engine, storage or browser API dependencies. */
let activeWallet: symbol | null = null;

export function claimWalletOwner(): symbol | null {
  if (activeWallet) return null;
  return activeWallet = Symbol("wallet owner");
}

export function releaseWalletOwner(lease: symbol): boolean {
  if (activeWallet !== lease) return false;
  activeWallet = null;
  return true;
}

/** Read-only diagnostic; does not claim ownership or inspect another tab. */
export function browserWalletActive(): boolean { return activeWallet !== null; }
