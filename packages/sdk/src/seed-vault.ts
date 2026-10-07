/** Passphrase-encrypted mnemonic in IndexedDB. Never stores plaintext seed. */

import { WalletError } from "@z-stack/core";
import { beginVaultOperation, invalidateVaultOperations } from "./vault-operation";
import { walletStorageAvailable, abortable, checkVaultGeneration, domainGenerationKey, walletTransaction, type WalletGeneration } from "./wallet-storage";

const SEED_KEY = "seed.enc";
const ITERATIONS_V1 = 210_000;
const ITERATIONS_V2 = 600_000;

type EncBlob = { v: 1 | 2; salt: string; iv: string; ct: string };

function b64(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s);
}

function unb64(s: string): Uint8Array {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

async function deriveKey(
  passphrase: string,
  salt: Uint8Array,
  iterations: number,
  signal?: AbortSignal,
): Promise<CryptoKey> {
  const enc = new TextEncoder();
  const base = await abortable(signal, () => crypto.subtle.importKey("raw", enc.encode(passphrase), "PBKDF2", false, [
    "deriveKey",
  ]));
  return abortable(signal, () => crypto.subtle.deriveKey(
    { name: "PBKDF2", salt: salt as BufferSource, iterations, hash: "SHA-256" },
    base,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"],
  ));
}

export async function persistEncryptedSeed(
  passphrase: string, mnemonic: string, signal?: AbortSignal,
  expectedGeneration?: WalletGeneration | Promise<WalletGeneration>,
): Promise<void> {
  if (!walletStorageAvailable() || typeof crypto === "undefined" || !crypto.subtle) {
    throw new Error("WebCrypto / IndexedDB required to persist the seed");
  }
  const op = beginVaultOperation("seed", signal, false, expectedGeneration);
  try {
    await op.ready();
    const salt = crypto.getRandomValues(new Uint8Array(16));
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const key = await deriveKey(passphrase, salt, ITERATIONS_V2, op.signal);
    const ct = new Uint8Array(await op.wait(() =>
      crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, new TextEncoder().encode(mnemonic))));
    const blob: EncBlob = { v: 2, salt: b64(salt), iv: b64(iv), ct: b64(ct) };
    const generation = await op.generation;
    op.assertCurrent();
    await walletTransaction<void>("readwrite", tx => {
      checkVaultGeneration(tx, "seed", generation, () => tx.store.put(blob, SEED_KEY));
    }, op.signal);
  } finally { op.dispose(); }
}

export async function hasEncryptedSeed(): Promise<boolean> {
  if (!walletStorageAvailable()) return false;
  return walletTransaction("readonly", tx => {
    tx.request(tx.store.get(SEED_KEY), value => tx.result(!!value));
  });
}

export async function unlockEncryptedSeed(passphrase: string, signal?: AbortSignal): Promise<string> {
  if (!walletStorageAvailable()) throw new Error("IndexedDB unavailable");
  const op = beginVaultOperation("seed", signal);
  try {
    const generation = await op.generation;
    const blob = await walletTransaction<EncBlob | undefined>("readonly", tx => {
      checkVaultGeneration(tx, "seed", generation, () => {
        tx.request(tx.store.get(SEED_KEY), value => tx.result(value as EncBlob | undefined));
      });
    }, op.signal);
    if (!blob || (blob.v !== 1 && blob.v !== 2)) throw new Error("no encrypted seed on this device");
    const key = await deriveKey(passphrase, unb64(blob.salt), blob.v === 2 ? ITERATIONS_V2 : ITERATIONS_V1, op.signal);
    let pt: ArrayBuffer;
    try {
      pt = await op.wait(() => crypto.subtle.decrypt(
        { name: "AES-GCM", iv: unb64(blob.iv) as BufferSource }, key, unb64(blob.ct) as BufferSource));
    } catch (error) {
      op.assertCurrent();
      throw new Error("wrong passphrase", { cause: error });
    }
    await walletTransaction<void>("readonly", tx => checkVaultGeneration(tx, "seed", generation, () => {}), op.signal);
    op.assertCurrent();
    return new TextDecoder().decode(pt);
  } finally { op.dispose(); }
}

export async function forgetEncryptedSeed(): Promise<void> {
  invalidateVaultOperations("seed");
  if (!walletStorageAvailable()) throw new WalletError("wallet_db", "Local seed storage is unavailable; deletion was not completed.");
  await walletTransaction<void>("readwrite", tx => {
    tx.store.delete(SEED_KEY);
    tx.store.put(crypto.randomUUID(), domainGenerationKey("seed"));
  });
}
