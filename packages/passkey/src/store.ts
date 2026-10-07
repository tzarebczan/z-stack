/**
 * Where vault records live. A record holds only ciphertext plus public
 * metadata (credential IDs, salts), so it is safe to store anywhere, including
 * a server for cross-device backup. Stores do compare-and-swap on `revision`
 * so two tabs cannot silently overwrite each other.
 */

import { PasskeyError, throwIfAborted } from "./errors";
import type { Sealed } from "./crypto";

export type StoredPasskey = {
  credentialId: string;
  transports?: string[];
  /** HKDF salt for this passkey's key-encryption key. */
  salt: string;
  /** The vault's data key, encrypted for this passkey. */
  wrapped: Sealed;
  name?: string;
  aaguid?: string;
  /** Authenticator reported the credential as sync-capable (BE flag). */
  backupEligible?: boolean;
  /** Authenticator reported the credential as synced (BS flag). */
  backedUp?: boolean;
  /** An encrypted copy of the vault was written to this passkey's largeBlob. */
  portable?: boolean;
  createdAt: number;
  lastUsedAt?: number;
};

export type PasskeyVaultRecord = {
  format: "z-stack/passkey-vault";
  version: 1;
  id: string;
  rpId: string;
  purpose: string;
  /** WebAuthn `user.id` (base64url) shared by every passkey of this vault. */
  userId: string;
  userName: string;
  /** Incremented on every write; stores reject stale writes. */
  revision: number;
  data: Sealed;
  passkeys: StoredPasskey[];
  /** Locally removed credentials must not rejoin through a portable copy. */
  revokedCredentialIds?: string[];
  createdAt: number;
  updatedAt: number;
  /** A `forget()` tombstone: no vault, but writes must still match its revision. */
  forgotten?: true;
};

export interface PasskeyVaultStore {
  /** Return saved tombstones too; undefined means no stored row, not a forgotten vault. */
  get(id: string, signal?: AbortSignal): Promise<PasskeyVaultRecord | undefined>;
  /**
   * Write `record` only if the stored revision is `expectedRevision`
   * (`undefined`: nothing stored). Otherwise throw `PasskeyError("conflict")`.
   */
  put(record: PasskeyVaultRecord, expectedRevision: number | undefined, signal?: AbortSignal): Promise<void>;
  /** Administrative hard-delete. Vault forget() writes a CAS tombstone instead. */
  delete(id: string, signal?: AbortSignal): Promise<void>;
}

function conflict(): PasskeyError {
  return new PasskeyError("conflict", "The vault changed in another tab. Reload and try again.");
}

/** In-memory store (tests, or apps that persist records themselves). */
export function memoryVaultStore(initial: PasskeyVaultRecord[] = []): PasskeyVaultStore {
  const records = new Map(initial.map((r) => [r.id, structuredClone(r)]));
  return {
    async get(id, signal) {
      throwIfAborted(signal);
      const r = records.get(id);
      return r ? structuredClone(r) : undefined;
    },
    async put(record, expectedRevision, signal) {
      throwIfAborted(signal);
      if (records.get(record.id)?.revision !== expectedRevision) throw conflict();
      records.set(record.id, structuredClone(record));
    },
    async delete(id, signal) {
      throwIfAborted(signal);
      records.delete(id);
    },
  };
}

/**
 * Opens of one database name run one at a time. Two callers that both still
 * need a store would otherwise read version N and both request N+1, and only
 * the first `onupgradeneeded` would run.
 */
const dbOpenQueue = new Map<string, Promise<void>>();

function enqueueDbOpen<T>(dbName: string, task: () => Promise<T>): Promise<T> {
  const previous = dbOpenQueue.get(dbName) ?? Promise.resolve();
  const run = previous.then(task, task);
  dbOpenQueue.set(
    dbName,
    run.then(
      () => undefined,
      () => undefined,
    ),
  );
  return run;
}

function retryableSchemaError(error: unknown): boolean {
  const name = error instanceof Error ? error.name : "";
  return name === "VersionError" || name === "NotFoundError";
}

/** IndexedDB store. One object store, keyed by vault id. */
export function indexedDbVaultStore(options: { dbName?: string; storeName?: string } = {}): PasskeyVaultStore {
  const dbName = options.dbName ?? "z-stack-passkey";
  const storeName = options.storeName ?? "vaults";

  function requestOpen(version: number | undefined, create: boolean): Promise<IDBDatabase> {
    return new Promise((resolve, reject) => {
      const req = version === undefined ? indexedDB.open(dbName) : indexedDB.open(dbName, version);
      if (create) {
        req.onupgradeneeded = () => {
          if (!req.result.objectStoreNames.contains(storeName)) req.result.createObjectStore(storeName);
        };
      }
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error ?? new Error("IndexedDB open failed"));
    });
  }

  async function openOnce(): Promise<IDBDatabase> {
    const existing = await requestOpen(undefined, false);
    const missing = !existing.objectStoreNames.contains(storeName);
    const version = missing ? existing.version + 1 : existing.version;
    existing.close();
    const db = await requestOpen(version, true);
    if (!db.objectStoreNames.contains(storeName)) {
      db.close();
      throw new DOMException(`Missing object store "${storeName}"`, "NotFoundError");
    }
    return db;
  }

  function openWithRetry(attempt = 0): Promise<IDBDatabase> {
    return openOnce().catch((error: unknown) => {
      if (!retryableSchemaError(error) || attempt >= 3) throw error;
      return openWithRetry(attempt + 1);
    });
  }

  function openDb(): Promise<IDBDatabase> {
    if (typeof indexedDB === "undefined") {
      return Promise.reject(new PasskeyError("unsupported", "IndexedDB is not available in this context."));
    }
    return enqueueDbOpen(dbName, () => openWithRetry());
  }

  async function run<T>(
    mode: IDBTransactionMode,
    body: (store: IDBObjectStore, done: (value: T) => void, fail: (error: unknown) => void) => void,
    signal?: AbortSignal,
  ): Promise<T> {
    throwIfAborted(signal);
    const db = await openDb();
    try {
      // The signal may have fired while the database was opening; the abort
      // listener below would miss it and the write would commit.
      throwIfAborted(signal);
      return await new Promise<T>((resolve, reject) => {
        const tx = db.transaction(storeName, mode);
        let value: T;
        let failure: unknown;
        const fail = (error: unknown) => {
          failure ??= error;
          try {
            tx.abort();
          } catch {
            reject(failure);
          }
        };
        const onAbort = () => fail(signal?.reason);
        signal?.addEventListener("abort", onAbort, { once: true });
        tx.oncomplete = () => {
          signal?.removeEventListener("abort", onAbort);
          if (failure) reject(failure);
          else resolve(value);
        };
        tx.onabort = () => {
          signal?.removeEventListener("abort", onAbort);
          reject(failure ?? tx.error ?? new Error("IndexedDB transaction aborted"));
        };
        body(tx.objectStore(storeName), (v) => (value = v), fail);
      });
    } catch (error) {
      if (signal?.aborted) throwIfAborted(signal);
      throw error;
    } finally {
      db.close();
    }
  }

  return {
    get(id, signal) {
      return run<PasskeyVaultRecord | undefined>("readonly", (store, done) => {
        const req = store.get(id);
        req.onsuccess = () => done(req.result as PasskeyVaultRecord | undefined);
      }, signal);
    },
    put(record, expectedRevision, signal) {
      return run<void>("readwrite", (store, done, fail) => {
        const req = store.get(record.id);
        req.onsuccess = () => {
          const current = req.result as PasskeyVaultRecord | undefined;
          if (current?.revision !== expectedRevision) return fail(conflict());
          store.put(record, record.id);
          done(undefined);
        };
      }, signal);
    },
    delete(id, signal) {
      return run<void>("readwrite", (store, done) => {
        store.delete(id);
        done(undefined);
      }, signal);
    },
  };
}
