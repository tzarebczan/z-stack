/** Transactional test double: request ordering, atomic commit/abort and observable value clones. */
export function memoryIndexedDb() {
  const records = new Map<IDBValidKey, unknown>();
  const reads: IDBValidKey[] = [];
  const keyReads: IDBValidKey[] = [];
  const writes: IDBValidKey[] = [];
  const opens: Array<() => void> = [];
  const commits: Array<() => void> = [];
  const completions: Array<() => void> = [];
  const waiting: Array<() => void> = [];
  let busy = false;
  const state = {
    records, reads, keyReads, writes, opens, commits, completions,
    holdCompletionEvents: false,
    holdOpens: false, holdCommits: false, holdReadCommits: false, failPutKey: null as IDBValidKey | null,
    failDeleteKey: null as IDBValidKey | null, failDeleteSyncKey: null as IDBValidKey | null,
    failTransaction: false, abortNextCommit: false, clonedBytes: 0, closes: 0, openCount: 0,
  };
  const next = () => { busy = false; waiting.shift()?.(); };
  const db = {
    close() { state.closes++; },
    objectStoreNames: { contains: () => true },
    transaction(_store: string, mode = "readonly") {
      if (state.failTransaction) { state.failTransaction = false; throw new Error("transaction creation failed"); }
      const queue: Array<() => void> = [];
      let local: Map<IDBValidKey, unknown>;
      let active = false;
      let ended = false;
      let scheduled = false;
      const tx: any = {
        oncomplete: null, onerror: null, onabort: null, error: null,
        abort() {
          if (ended) throw new Error("transaction finished");
          ended = true;
          queueMicrotask(() => { tx.onabort?.(); if (active) next(); });
        },
        objectStore: () => ({
          get: (key: IDBValidKey) => request(() => {
            reads.push(key);
            const value = local.get(key);
            if (ArrayBuffer.isView(value)) state.clonedBytes += value.byteLength;
            if (value instanceof ArrayBuffer) state.clonedBytes += value.byteLength;
            return structuredClone(value);
          }),
          getKey: (key: IDBValidKey) => request(() => {
            keyReads.push(key);
            return local.has(key) ? key : undefined;
          }),
          put: (value: unknown, key: IDBValidKey) => {
            if (mode !== "readwrite") throw new DOMException("readonly transaction", "ReadOnlyError");
            // IndexedDB clones synchronously at the call boundary.
            const copy = structuredClone(value);
            return request(() => {
              if (state.failPutKey === key) { state.failPutKey = null; throw new Error("quota exceeded"); }
              writes.push(key); local.set(key, copy); return key;
            });
          },
          delete: (key: IDBValidKey | { lower: string; upper: string }) => {
            if (mode !== "readwrite") throw new DOMException("readonly transaction", "ReadOnlyError");
            if (state.failDeleteSyncKey === key) { state.failDeleteSyncKey = null; throw new Error("synchronous delete failed"); }
            return request(() => {
            if (state.failDeleteKey === key) { state.failDeleteKey = null; throw new Error("delete failed"); }
            if (typeof key === "object" && "lower" in key) {
              for (const value of local.keys()) {
                if (typeof value === "string" && value >= key.lower && value <= key.upper) local.delete(value);
              }
            } else local.delete(key as IDBValidKey);
            });
          },
        }),
      };
      const commit = () => {
        if (ended) return;
        if (state.abortNextCommit) { state.abortNextCommit = false; tx.abort(); return; }
        ended = true;
        if (mode === "readwrite") {
          records.clear();
          for (const [key, value] of local) records.set(key, value);
        }
        const complete = () => { tx.oncomplete?.(); next(); };
        if (mode === "readwrite" && state.holdCompletionEvents) completions.push(complete);
        else complete();
      };
      const drain = () => {
        scheduled = false;
        if (!active || ended) return;
        const step = queue.shift();
        if (step) { step(); schedule(); return; }
        if ((state.holdCommits && mode === "readwrite") || (state.holdReadCommits && mode === "readonly")) commits.push(commit);
        else commit();
      };
      const schedule = () => {
        if (!scheduled && active && !ended) { scheduled = true; queueMicrotask(drain); }
      };
      function request(run: () => unknown) {
        const req: any = { result: undefined, error: null, onsuccess: null, onerror: null };
        queue.push(() => {
          try { req.result = run(); req.onsuccess?.(); }
          catch (e) {
            req.error = e; tx.error = e; req.onerror?.(); tx.onerror?.(); tx.abort();
          }
        });
        schedule();
        return req;
      }
      const start = () => {
        if (ended) { next(); return; }
        busy = true; active = true; local = new Map(records); schedule();
      };
      if (busy) waiting.push(start); else start();
      return tx;
    },
  };
  const indexedDB = {
    open() {
      state.openCount++;
      const req: any = { result: db, onsuccess: null, onerror: null };
      const ready = () => req.onsuccess?.();
      if (state.holdOpens) opens.push(ready); else queueMicrotask(ready);
      return req;
    },
  };
  return { ...state, get closes() { return state.closes; }, get clonedBytes() { return state.clonedBytes; },
    get openCount() { return state.openCount; },
    set holdOpens(value: boolean) { state.holdOpens = value; },
    set holdCommits(value: boolean) { state.holdCommits = value; },
    set holdCompletionEvents(value: boolean) { state.holdCompletionEvents = value; },
    set holdReadCommits(value: boolean) { state.holdReadCommits = value; },
    set failPutKey(value: IDBValidKey | null) { state.failPutKey = value; },
    set failDeleteKey(value: IDBValidKey | null) { state.failDeleteKey = value; },
    set failDeleteSyncKey(value: IDBValidKey | null) { state.failDeleteSyncKey = value; },
    set failTransaction(value: boolean) { state.failTransaction = value; },
    set abortNextCommit(value: boolean) { state.abortNextCommit = value; },
    indexedDB: indexedDB as unknown as IDBFactory,
    IDBKeyRange: { bound: (lower: string, upper: string) => ({ lower, upper }) },
  };
}
