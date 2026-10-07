import { WalletError } from "@z-stack/core";
import { indexedDbWalletStorage, type WalletStorage } from "./storage";
import { aborted, checkSignal } from "./abort";
export { aborted, checkSignal, abortable } from "./abort";

let storage = indexedDbWalletStorage();
/** Internal: changed only while the public wallet lease is exclusively held. */
export function useWalletStorage(next?: WalletStorage): void { storage = next ?? indexedDbWalletStorage(); }
export function walletStorageAvailable(): boolean { return storage.available; }

/** Shared transaction plumbing. Callbacks must queue requests synchronously. */
export const WALLET_STORE = "wallets";
export const WALLET_GENERATION_KEY = "wallet:generation";
export type WalletGeneration = string;
export type VaultDomain = "seed" | "passkey";
const INITIAL_GENERATION = "initial";

export function openWalletDb(signal?: AbortSignal): Promise<IDBDatabase> {
  checkSignal(signal);
  return new Promise((resolve, reject) => {
    const req = indexedDB.open("z-stack-wasm", 1);
    let cancelled = false;
    const cleanup = () => signal?.removeEventListener("abort", cancel);
    const cancel = () => { cancelled = true; cleanup(); reject(signal?.reason ?? aborted()); };
    signal?.addEventListener("abort", cancel, { once: true });
    req.onupgradeneeded = () => {
      if (!req.result.objectStoreNames.contains(WALLET_STORE)) req.result.createObjectStore(WALLET_STORE);
    };
    req.onsuccess = () => {
      cleanup();
      if (cancelled || signal?.aborted) { req.result.close(); reject(signal?.reason ?? aborted()); }
      else resolve(req.result);
    };
    req.onerror = () => { cleanup(); reject(req.error ?? new Error("IndexedDB open failed")); };
  });
}

type StorageRequest<T> = { run(receive: (value: T) => void): void };
export type WalletRecordStore = {
  get(key: string): StorageRequest<unknown>;
  getKey(key: string): StorageRequest<string | undefined>;
  put(value: unknown, key: string): void;
  delete(key: string): void;
  deletePrefix(prefix: string): void;
};
export type WalletTransaction<T> = {
  store: WalletRecordStore;
  result(value: T): void;
  request<V>(request: StorageRequest<V>, success: (value: V) => void): void;
  fail(error: unknown): void;
};

export async function walletTransaction<T>(
  mode: "readonly" | "readwrite",
  body: (tx: WalletTransaction<T>) => void,
  signal?: AbortSignal,
  /** A final replacement is successful once IDB committed, even if abort arrived before oncomplete. */
  commitWinsCancellation = false,
): Promise<T> {
  checkSignal(signal);
  return storage.transaction<T>(mode, tx => body({
    store: {
      get: key => ({ run: receive => tx.get(key, receive) }),
      getKey: key => ({ run: receive => tx.has(key, exists => receive(exists ? key : undefined)) }),
      put: (value, key) => tx.put(key, value),
      delete: key => tx.delete(key),
      deletePrefix: prefix => tx.deletePrefix(prefix),
    },
    result: value => tx.result(value),
    fail: error => tx.fail(error),
    request: (request, receive) => request.run(value => { checkSignal(signal); receive(value); }),
  }), { signal, commitWinsCancellation });
}

export function generationValue(value: unknown): string {
  if (value === undefined) return INITIAL_GENERATION;
  if (typeof value !== "string" || !value) throw new Error("invalid wallet generation");
  return value;
}

export function domainGenerationKey(domain: VaultDomain): string { return `vault:${domain}:generation`; }

export function readWalletGeneration(signal?: AbortSignal): Promise<string> {
  checkSignal(signal);
  if (!walletStorageAvailable()) return Promise.resolve(INITIAL_GENERATION);
  return walletTransaction("readonly", tx => {
    tx.request(tx.store.get(WALLET_GENERATION_KEY), value => tx.result(generationValue(value)));
  }, signal);
}

/** Explicitly fence an old session. Wallet replacements prepare and commit via snapshot-storage. */
export function advanceWalletGeneration(signal?: AbortSignal, expectedGeneration?: WalletGeneration, allowReplace = true): Promise<string> {
  checkSignal(signal);
  if (!walletStorageAvailable()) return Promise.resolve(INITIAL_GENERATION);
  return walletTransaction("readwrite", tx => {
    tx.request(tx.store.get(WALLET_GENERATION_KEY), value => {
      if (expectedGeneration !== undefined && generationValue(value) !== expectedGeneration) {
        throw new WalletError("wallet_changed", "saved wallet changed; retry the operation");
      }
      tx.request(tx.store.get("default"), saved => {
        if (!allowReplace && saved !== undefined && saved !== null) {
          throw new WalletError("already_exists", "A saved wallet exists. Confirm replacement explicitly.");
        }
        const generation = crypto.randomUUID();
        tx.store.put(generation, WALLET_GENERATION_KEY);
        tx.result(generation);
      });
    });
  // Return a committed claim even if cancellation raced its completion event;
  // the caller needs that identity to undo its exact claim safely.
  }, signal, true);
}

export type VaultGeneration = { wallet: string; domain: string };

export function readVaultGeneration(domain: VaultDomain, signal?: AbortSignal): Promise<VaultGeneration> {
  return walletTransaction("readonly", tx => {
    tx.request(tx.store.get(WALLET_GENERATION_KEY), wallet => {
      tx.request(tx.store.get(domainGenerationKey(domain)), value => {
        tx.result({ wallet: generationValue(wallet), domain: generationValue(value) });
      });
    });
  }, signal);
}

export function checkVaultGeneration<T>(
  tx: WalletTransaction<T>, domain: VaultDomain, expected: VaultGeneration, next: () => void,
): void {
  tx.request(tx.store.get(WALLET_GENERATION_KEY), wallet => {
    tx.request(tx.store.get(domainGenerationKey(domain)), value => {
      if (generationValue(wallet) !== expected.wallet || generationValue(value) !== expected.domain) {
        throw aborted("saved wallet changed; retry the operation");
      }
      next();
    });
  });
}
