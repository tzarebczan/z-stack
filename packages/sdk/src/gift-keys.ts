/** Rust key operations for optional bearer-gift integrations. No wallet is replaced. */
import { deriveAccount, generateMnemonic, giftMnemonicToEntropy, giftMnemonicFromEntropy,
  seedFingerprint, unifiedAddressForSet, initialize, type Network, type SdkInitOptions } from "./runtime";

export { giftMnemonicFromEntropy, giftMnemonicToEntropy } from "./runtime";
export type GiftAccount = { entropy: Uint8Array; address: string; fingerprint: string };

/** Initialize only the shared engine; this does not claim or create a wallet. */
export async function initializeGiftKeys(options: SdkInitOptions = {}): Promise<void> {
  await initialize(options);
}

export function deriveGiftAccount(entropy: Uint8Array, network: Network): GiftAccount {
  const words = giftMnemonicFromEntropy(entropy);
  const account = deriveAccount(words, network, 0);
  return { entropy: entropy.slice(), fingerprint: seedFingerprint(words),
    address: unifiedAddressForSet(account.ufvk, network, "shielded") };
}

export function createGiftAccount(network: Network): GiftAccount {
  const entropy = giftMnemonicToEntropy(generateMnemonic());
  try { return deriveGiftAccount(entropy, network); }
  finally { entropy.fill(0); }
}
