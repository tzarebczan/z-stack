/** Local transactional wallet storage. Contains viewing data, never a plaintext seed. */
export interface WalletStorageTransaction<T> {
  /** Queue a read; callbacks may synchronously queue further operations. */
  get(key: string, receive: (value: unknown) => void): void;
  has(key: string, receive: (exists: boolean) => void): void;
  put(key: string, value: unknown): void;
  delete(key: string): void;
  deletePrefix(prefix: string): void;
  result(value: T): void;
  fail(error: unknown): void;
}

export interface WalletStorage {
  readonly available: boolean;
  /**
   * Serialize overlapping transactions across all clients sharing this store.
   * Reads and every queued write (including chunk/manifest and generation CAS)
   * are atomic. A failed/aborted transaction changes nothing. Resolve only after
   * commit, not after queuing writes. The body and read callbacks are synchronous;
   * don't perform network or unrelated asynchronous work inside them.
   *
   * commitWinsCancellation: if commit physically won a cancellation race, return
   * the committed result. Never report rollback after durable replacement bytes
   * were written. Generation tombstones must survive deletion and app restarts.
   */
  transaction<T>(mode: "readonly" | "readwrite", body: (tx: WalletStorageTransaction<T>) => void,
    options?: { signal?: AbortSignal; commitWinsCancellation?: boolean }): Promise<T>;
}

/** Browser storage, with an application-owned namespace and optional IDB implementation. */
export function indexedDbWalletStorage(options: { name?: string; factory?: IDBFactory } = {}): WalletStorage {
  const name = options.name ?? "z-stack-wasm";
  if (!name.trim()) throw new TypeError("Wallet database name is required.");
  const factory = () => options.factory ?? (typeof indexedDB === "undefined" ? undefined : indexedDB);
  return {
    get available() { return !!factory(); },
    async transaction<T>(mode: "readonly" | "readwrite", body: (tx: WalletStorageTransaction<T>) => void,
      opts: { signal?: AbortSignal; commitWinsCancellation?: boolean } = {}): Promise<T> {
      const { signal, commitWinsCancellation = false } = opts;
      signal?.throwIfAborted();
      const idb = factory();
      if (!idb) throw new Error("IndexedDB unavailable; supply a WalletStorage adapter.");
      const db = await new Promise<IDBDatabase>((resolve, reject) => {
        const request = idb.open(name, 1);
        const cancel = () => reject(signal?.reason);
        signal?.addEventListener("abort", cancel, { once: true });
        const cleanup = () => signal?.removeEventListener("abort", cancel);
        request.onupgradeneeded = () => {
          if (!request.result.objectStoreNames.contains("wallets")) request.result.createObjectStore("wallets");
        };
        request.onsuccess = () => {
          cleanup();
          if (signal?.aborted) { request.result.close(); reject(signal.reason); }
          else resolve(request.result);
        };
        request.onerror = () => { cleanup(); reject(request.error); };
      });
      try {
        signal?.throwIfAborted();
        return await new Promise<T>((resolve, reject) => {
          const native = db.transaction("wallets", mode);
          const store = native.objectStore("wallets");
          let value: T;
          let failure: unknown;
          const cleanup = () => signal?.removeEventListener("abort", cancel);
          const fail = (error: unknown) => {
            failure ??= error;
            try { native.abort(); }
            catch { if (!commitWinsCancellation) { cleanup(); reject(failure); } }
          };
          const cancel = () => fail(signal?.reason);
          const read = <V>(request: IDBRequest<V>, receive: (value: V) => void) => {
            request.onsuccess = () => {
              try { signal?.throwIfAborted(); receive(request.result); }
              catch (error) { fail(error); }
            };
          };
          native.oncomplete = () => {
            cleanup();
            if (commitWinsCancellation) resolve(value);
            else if (signal?.aborted) reject(signal.reason);
            else if (failure) reject(failure);
            else resolve(value);
          };
          native.onabort = () => { cleanup(); reject(failure ?? native.error ?? new DOMException("Storage transaction aborted", "AbortError")); };
          native.onerror = () => { failure ??= native.error ?? undefined; };
          signal?.addEventListener("abort", cancel, { once: true });
          try {
            signal?.throwIfAborted();
            body({
              get: (key, receive) => read(store.get(key), receive),
              has: (key, receive) => read(store.getKey(key), key => receive(key !== undefined)),
              put: (key, value) => { store.put(value, key); },
              delete: key => { store.delete(key); },
              deletePrefix: prefix => { store.delete(IDBKeyRange.bound(prefix, `${prefix}\uffff`)); },
              result: next => { value = next; }, fail,
            });
          } catch (error) { fail(error); }
        });
      } finally { db.close(); }
    },
  };
}

/** Ephemeral storage for tests and disposable wallets. Closing it does not make it durable. */
export function memoryWalletStorage(): WalletStorage {
  let records = new Map<string, unknown>();
  let tail: Promise<unknown> = Promise.resolve();
  return {
    available: true,
    transaction<T>(mode: "readonly" | "readwrite", body: (tx: WalletStorageTransaction<T>) => void,
      opts: { signal?: AbortSignal; commitWinsCancellation?: boolean } = {}): Promise<T> {
      const work = tail.catch(() => {}).then(() => {
        opts.signal?.throwIfAborted();
        const draft = new Map(records);
        let result: T;
        const write = (action: () => void) => {
          if (mode !== "readwrite") throw new Error("Cannot write in a readonly transaction.");
          opts.signal?.throwIfAborted(); action();
        };
        body({
          get: (key, receive) => receive(structuredClone(draft.get(key))),
          has: (key, receive) => receive(draft.has(key)),
          put: (key, value) => write(() => { draft.set(key, structuredClone(value)); }),
          delete: key => write(() => { draft.delete(key); }),
          deletePrefix: prefix => write(() => { for (const key of draft.keys()) if (key.startsWith(prefix)) draft.delete(key); }),
          result: value => { result = value; }, fail: error => { throw error; },
        });
        opts.signal?.throwIfAborted();
        if (mode === "readwrite") records = draft;
        return result!;
      });
      tail = work;
      return work;
    },
  };
}
