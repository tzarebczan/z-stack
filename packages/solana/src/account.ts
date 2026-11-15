import { mnemonicToSeedSync, validateMnemonic } from '@scure/bip39';
import { wordlist } from '@scure/bip39/wordlists/english.js';
import { HDKey } from 'micro-key-producer/slip10.js';
import { ed25519 } from '@noble/curves/ed25519.js';
import { Keypair, PublicKey } from '@solana/web3.js';
import bs58 from 'bs58';

export const SOLANA_DERIVATION_PATH = "m/44'/501'/0'/0'";
function signingKey(mnemonic: string) {
  const words = mnemonic.normalize('NFKD').trim().split(/\s+/).join(' ');
  if (!validateMnemonic(words, wordlist)) throw new Error('Invalid recovery phrase.');
  const seed = mnemonicToSeedSync(words), root = HDKey.fromMasterSeed(seed), child = root.derive(SOLANA_DERIVATION_PATH);
  const publicKey = ed25519.getPublicKey(child.privateKey), secretKey = new Uint8Array(64);
  secretKey.set(child.privateKey); secretKey.set(publicKey, 32);
  const signer = new Keypair({ publicKey, secretKey });
  seed.fill(0); root.privateKey.fill(0); root.chainCode.fill(0); child.privateKey.fill(0); child.chainCode.fill(0);
  return { signer, destroy() { secretKey.fill(0); } };
}
/** Caller must not retain this signer. JavaScript cannot guarantee complete memory erasure. */
export async function withSolanaSigner<T>(mnemonic: string, run: (signer: Keypair) => T | Promise<T>): Promise<T> {
  const key = signingKey(mnemonic);
  try { return await run(key.signer); } finally { key.destroy(); }
}
export function deriveSolanaAddress(mnemonic: string): string {
  const key = signingKey(mnemonic);
  try { return key.signer.publicKey.toBase58(); } finally { key.destroy(); }
}
export function verifySolanaSignature(owner: string, message: Uint8Array, signature: string | Uint8Array): boolean {
  try { return ed25519.verify(typeof signature === 'string' ? bs58.decode(signature) : signature, message, new PublicKey(owner).toBytes()); }
  catch { return false; }
}
export function signSolanaMessage(signer: Keypair | string, message: Uint8Array): string {
  if (typeof signer === 'string') {
    const key = signingKey(signer);
    try { return signSolanaMessage(key.signer, message); } finally { key.destroy(); }
  }
  const secret = signer.secretKey;
  try { return bs58.encode(ed25519.sign(message, secret.subarray(0, 32))); } finally { secret.fill(0); }
}
