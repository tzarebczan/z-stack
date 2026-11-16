import { WalletError } from "@z-stack/core";
import { indexedDbWalletStorage, type WalletStorage, type WalletStorageTransaction } from "./storage";

/**
 * Optional encryption at rest for a {@link WalletStorage}.
 *
 * Every record is sealed with AES-256-GCM under a caller-supplied key, a fresh
 * random 96-bit nonce per write, and associated data binding the store, the
 * record name and the commit sequence. A small encrypted head record lists the
 * sequence of every overwritable record, so a swapped, deleted, replayed or
 * altered record fails verification. Record names are unchanged: they are
 * fixed labels and random identifiers, never addresses, keys or fingerprints.
 *
 * The SDK never derives, stores or logs the key. If it is lost, the store
 * cannot be opened: erase it and restore the wallet from its recovery phrase.
 */
export type WalletStoreKey = Uint8Array | CryptoKey;

/** Without a key: does a saved wallet exist, and is the store encrypted? */
export type WalletStoreStatus = { exists: boolean; encrypted: boolean };

/** Plaintext marker: identifies an encrypted store and checks a key. No wallet data. */
const MARKER_KEY = "z-stack:encryption";
const HEAD_KEY = "z-stack:encryption:head";
const FORMAT = "z-stack-store-aes-256-gcm-1";
/**
 * Snapshot records are written once under fresh random names and never
 * overwritten, so their name binding alone rules out replay. The head tracks
 * every other record. Keep this true when changing snapshot-storage.
 */
const WRITE_ONCE_PREFIX = "snapshot:";
const SAVED_WALLET_KEY = "default";
const GENERATION_KEY = "wallet:generation";
const MAX_ROUNDS = 256;

type Bytes = Uint8Array<ArrayBuffer>;
type Envelope = { zse: 1; seq: number; iv: Bytes; ct: Bytes };
type Marker = { format: string; storeId: string; iv: Bytes; check: Bytes };
type Head = { seq: number; keys: Record<string, number> };
type Meta = { markerRaw: unknown; headRaw: unknown; storeId?: string; head?: Head };
type Op = { put: string; bytes: Bytes } | { remove: string } | { prefix: string };

const ENCRYPTED = Symbol("encrypted wallet storage");

const locked = () => new WalletError("store_locked", "The wallet store is encrypted; supply its key.");
const rejected = () => new WalletError("store_key_rejected", "The supplied key does not open this wallet store.");
const notEncrypted = () => new WalletError("store_not_encrypted", "The wallet store is not encrypted; migrate it with encryptWalletStore first.");
const corrupt = (why: string) => new WalletError("store_corrupt", `Encrypted wallet store failed verification: ${why}.`);
const changing = () => new WalletError("wallet_changed", "The wallet store kept changing; retry the operation.");

function isEnvelope(value: unknown): value is Envelope {
  const v = value as Envelope;
  return !!v && typeof v === "object" && v.zse === 1 && Number.isSafeInteger(v.seq) && v.seq > 0
    && v.iv instanceof Uint8Array && v.iv.byteLength === 12 && v.ct instanceof Uint8Array && v.ct.byteLength >= 16;
}

function branded(value: unknown): boolean {
  return !!value && typeof value === "object" && "zse" in value;
}

function reserved(key: string): boolean {
  return key === MARKER_KEY || key.startsWith(`${MARKER_KEY}:`);
}

function writeOnce(key: string): boolean { return key.startsWith(WRITE_ONCE_PREFIX); }

function sameBytes(a: Uint8Array, b: Uint8Array): boolean {
  if (a.byteLength !== b.byteLength) return false;
  for (let i = 0; i < a.byteLength; i++) if (a[i] !== b[i]) return false;
  return true;
}

/** Identity of a stored marker or envelope: each write has a fresh random nonce. */
function sameRaw(a: unknown, b: unknown): boolean {
  if (a === undefined || b === undefined) return a === b;
  const x = a as Partial<Envelope & Marker>, y = b as Partial<Envelope & Marker>;
  return x.iv instanceof Uint8Array && y.iv instanceof Uint8Array && sameBytes(x.iv, y.iv)
    && x.seq === y.seq && x.storeId === y.storeId;
}

// Tagged binary encoding of the structured-clone subset the SDK stores.
const T_UNDEFINED = 0, T_NULL = 1, T_FALSE = 2, T_TRUE = 3, T_NUMBER = 4, T_STRING = 5,
  T_BYTES = 6, T_BUFFER = 7, T_ARRAY = 8, T_OBJECT = 9;
const MAX_DEPTH = 64;

function encodeValue(value: unknown): Bytes {
  const text = new TextEncoder();
  const parts: Uint8Array[] = [];
  let size = 0;
  const push = (bytes: Uint8Array) => { parts.push(bytes); size += bytes.byteLength; };
  const head = (tag: number, length?: number) => {
    const bytes = new Uint8Array(length === undefined ? 1 : 5);
    bytes[0] = tag;
    if (length !== undefined) new DataView(bytes.buffer).setUint32(1, length);
    push(bytes);
  };
  const visit = (v: unknown, depth: number): void => {
    if (depth > MAX_DEPTH) throw new TypeError("Wallet store value nests too deeply.");
    if (v === undefined) head(T_UNDEFINED);
    else if (v === null) head(T_NULL);
    else if (typeof v === "boolean") head(v ? T_TRUE : T_FALSE);
    else if (typeof v === "number") {
      const bytes = new Uint8Array(9);
      bytes[0] = T_NUMBER;
      new DataView(bytes.buffer).setFloat64(1, v);
      push(bytes);
    } else if (typeof v === "string") {
      const bytes = text.encode(v);
      head(T_STRING, bytes.byteLength); push(bytes);
    } else if (v instanceof Uint8Array) { head(T_BYTES, v.byteLength); push(v); }
    else if (v instanceof ArrayBuffer) { head(T_BUFFER, v.byteLength); push(new Uint8Array(v)); }
    else if (Array.isArray(v)) {
      head(T_ARRAY, v.length);
      for (let i = 0; i < v.length; i++) visit(v[i], depth + 1);
    } else if (typeof v === "object" && [Object.prototype, null].includes(Object.getPrototypeOf(v))) {
      const entries = Object.entries(v);
      head(T_OBJECT, entries.length);
      for (const [key, item] of entries) {
        const name = text.encode(key);
        const length = new Uint8Array(4);
        new DataView(length.buffer).setUint32(0, name.byteLength);
        push(length); push(name); visit(item, depth + 1);
      }
    } else throw new TypeError("Encrypted wallet storage stores plain objects, arrays, strings, numbers, booleans and bytes only.");
  };
  visit(value, 0);
  const out = new Uint8Array(size);
  let offset = 0;
  for (const part of parts) { out.set(part, offset); offset += part.byteLength; }
  return out;
}

function decodeValue(bytes: Uint8Array): unknown {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const text = new TextDecoder("utf-8", { fatal: true });
  let offset = 0;
  const bad = (): never => { throw corrupt("malformed record"); };
  const take = (n: number) => {
    if (n > bytes.byteLength - offset) bad();
    const out = bytes.subarray(offset, offset + n);
    offset += n;
    return out;
  };
  const u32 = () => { if (offset + 4 > bytes.byteLength) bad(); const n = view.getUint32(offset); offset += 4; return n; };
  const count = () => { const n = u32(); if (n > bytes.byteLength - offset) bad(); return n; };
  const visit = (depth: number): unknown => {
    if (depth > MAX_DEPTH || offset >= bytes.byteLength) bad();
    const tag = bytes[offset++];
    switch (tag) {
      case T_UNDEFINED: return undefined;
      case T_NULL: return null;
      case T_FALSE: return false;
      case T_TRUE: return true;
      case T_NUMBER: { if (offset + 8 > bytes.byteLength) bad(); const n = view.getFloat64(offset); offset += 8; return n; }
      case T_STRING: return text.decode(take(u32()));
      case T_BYTES: return take(u32()).slice();
      case T_BUFFER: return take(u32()).slice().buffer;
      case T_ARRAY: {
        const out = new Array(count());
        for (let i = 0; i < out.length; i++) out[i] = visit(depth + 1);
        return out;
      }
      case T_OBJECT: {
        const out: Record<string, unknown> = {};
        for (let i = count(); i > 0; i--) {
          const key = text.decode(take(u32()));
          Object.defineProperty(out, key, { value: visit(depth + 1), enumerable: true, writable: true, configurable: true });
        }
        return out;
      }
      default: return bad();
    }
  };
  const value = visit(0);
  if (offset !== bytes.byteLength) bad();
  return value;
}

function subtle(): SubtleCrypto {
  const value = globalThis.crypto?.subtle;
  if (!value) throw new WalletError("wallet_db", "WebCrypto is unavailable; encrypted wallet storage needs a secure context.");
  return value;
}

/** Validates and imports without retaining the caller's bytes. */
function importStoreKey(key: WalletStoreKey): Promise<CryptoKey> {
  if (key instanceof Uint8Array) {
    if (key.byteLength !== 32) throw new TypeError("A wallet store key is exactly 32 bytes.");
    return subtle().importKey("raw", key.slice(), { name: "AES-GCM" }, false, ["encrypt", "decrypt"]);
  }
  if (typeof CryptoKey !== "undefined" && key instanceof CryptoKey) {
    const algorithm = key.algorithm as AesKeyAlgorithm;
    if (algorithm.name !== "AES-GCM" || algorithm.length !== 256 || !key.usages.includes("encrypt") || !key.usages.includes("decrypt")) {
      throw new TypeError("A wallet store CryptoKey must be AES-GCM, 256-bit, with encrypt and decrypt usages.");
    }
    return Promise.resolve(key);
  }
  throw new TypeError("A wallet store key is a 32-byte Uint8Array or an AES-GCM CryptoKey.");
}

function associatedData(storeId: string, name: string, seq: number): Bytes {
  return new TextEncoder().encode(JSON.stringify([FORMAT, storeId, name, seq])) as Bytes;
}

async function seal(key: CryptoKey, storeId: string, name: string, seq: number, plaintext: Bytes): Promise<Envelope> {
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const ct = new Uint8Array(await subtle().encrypt({ name: "AES-GCM", iv, additionalData: associatedData(storeId, name, seq) }, key, plaintext));
  return { zse: 1, seq, iv, ct };
}

async function unseal(key: CryptoKey, storeId: string, name: string, envelope: Envelope): Promise<Uint8Array | null> {
  try {
    return new Uint8Array(await subtle().decrypt({ name: "AES-GCM", iv: envelope.iv, additionalData: associatedData(storeId, name, envelope.seq) }, key, envelope.ct));
  } catch { return null; }
}

async function newMarker(key: CryptoKey, storeId: string): Promise<Marker> {
  const check = await seal(key, storeId, MARKER_KEY, 1, new Uint8Array(0));
  return { format: FORMAT, storeId, iv: check.iv, check: check.ct };
}

/** Verify the marker and head that were read together in one transaction. */
async function readMeta(key: CryptoKey, markerRaw: unknown, headRaw: unknown): Promise<Meta> {
  if (markerRaw === undefined) {
    if (headRaw !== undefined) throw corrupt("missing marker");
    return { markerRaw, headRaw };
  }
  const marker = markerRaw as Marker;
  if (!marker || typeof marker !== "object" || marker.format !== FORMAT || typeof marker.storeId !== "string"
    || !(marker.iv instanceof Uint8Array) || !(marker.check instanceof Uint8Array)) throw corrupt("unsupported marker");
  if (!await unseal(key, marker.storeId, MARKER_KEY, { zse: 1, seq: 1, iv: marker.iv, ct: marker.check })) throw rejected();
  if (!isEnvelope(headRaw)) throw corrupt("missing head");
  const plaintext = await unseal(key, marker.storeId, HEAD_KEY, headRaw);
  if (!plaintext) throw corrupt("head authentication failed");
  const head = decodeValue(plaintext) as Head;
  if (!head || head.seq !== headRaw.seq || !head.keys || typeof head.keys !== "object"
    || Object.values(head.keys).some(seq => !Number.isSafeInteger(seq) || seq < 1 || seq > head.seq)) throw corrupt("invalid head");
  return { markerRaw, headRaw, storeId: marker.storeId, head };
}

/** Plaintext bytes of one record, verified against the head read in the same transaction. */
async function readRecord(key: CryptoKey, meta: Meta, name: string, raw: unknown): Promise<Uint8Array | undefined> {
  if (!meta.head || !meta.storeId) {
    if (raw === undefined) return undefined;
    if (branded(raw)) throw corrupt("record without marker");
    throw notEncrypted();
  }
  const expected = writeOnce(name) ? undefined : meta.head.keys[name];
  if (raw === undefined) {
    if (expected !== undefined) throw corrupt("missing record");
    return undefined;
  }
  if (!isEnvelope(raw)) throw corrupt("unencrypted record");
  if (writeOnce(name) ? raw.seq > meta.head.seq : raw.seq !== expected) throw corrupt("stale or replayed record");
  const plaintext = await unseal(key, meta.storeId, name, raw);
  if (!plaintext) throw corrupt("record authentication failed");
  return plaintext;
}

type Fetched = { markerRaw: unknown; headRaw: unknown; values: Map<string, unknown> };

function fetchRecords(inner: WalletStorage, names: string[], signal?: AbortSignal): Promise<Fetched> {
  return inner.transaction<Fetched>("readonly", tx => {
    const fetched: Fetched = { markerRaw: undefined, headRaw: undefined, values: new Map() };
    let pending = names.length + 2;
    const done = () => { if (--pending === 0) tx.result(fetched); };
    tx.get(MARKER_KEY, value => { fetched.markerRaw = value; done(); });
    tx.get(HEAD_KEY, value => { fetched.headRaw = value; done(); });
    for (const name of names) tx.get(name, value => { fetched.values.set(name, value); done(); });
  }, { signal });
}

type DryRun<T> = { misses: Set<string>; ops: Op[]; value?: T; error?: { cause: unknown } };

/**
 * Run the synchronous body against verified plaintext. Callbacks fire in
 * request order, as in IndexedDB; the first unavailable record defers every
 * later callback, and the caller fetches the missing records and runs again.
 */
function dryRun<T>(mode: "readonly" | "readwrite", body: (tx: WalletStorageTransaction<T>) => void,
  plain: Map<string, Uint8Array | undefined>, signal?: AbortSignal): DryRun<T> {
  const run: DryRun<T> = { misses: new Set(), ops: [] };
  const overlay = new Map<string, Uint8Array | null>();
  const prefixes: string[] = [];
  const queue: Array<{ key: string; receive: (bytes: Uint8Array | undefined) => void }> = [];
  const guard = (key: string) => { if (reserved(key)) throw new TypeError("Reserved wallet store record name."); };
  const write = (action: () => void) => {
    if (mode !== "readwrite") throw new Error("Cannot write in a readonly transaction.");
    signal?.throwIfAborted();
    action();
  };
  const tx: WalletStorageTransaction<T> = {
    get: (key, receive) => { guard(key); queue.push({ key, receive: bytes => receive(bytes === undefined ? undefined : decodeValue(bytes)) }); },
    has: (key, receive) => { guard(key); queue.push({ key, receive: bytes => receive(bytes !== undefined) }); },
    put: (key, value) => write(() => { guard(key); const bytes = encodeValue(value); overlay.set(key, bytes); run.ops.push({ put: key, bytes }); }),
    delete: key => write(() => { guard(key); overlay.set(key, null); run.ops.push({ remove: key }); }),
    deletePrefix: prefix => write(() => {
      for (const key of overlay.keys()) if (key.startsWith(prefix)) overlay.set(key, null);
      prefixes.push(prefix);
      run.ops.push({ prefix });
    }),
    result: value => { run.value = value; },
    fail: error => { throw error; },
  };
  try {
    signal?.throwIfAborted();
    body(tx);
    let deferred = false;
    for (let item = queue.shift(); item; item = queue.shift()) {
      const known = overlay.has(item.key) || prefixes.some(prefix => item!.key.startsWith(prefix)) || plain.has(item.key);
      if (!known) { run.misses.add(item.key); deferred = true; }
      if (deferred) continue;
      signal?.throwIfAborted();
      const bytes = overlay.has(item.key) ? overlay.get(item.key) ?? undefined
        : prefixes.some(prefix => item!.key.startsWith(prefix)) ? undefined : plain.get(item.key);
      item.receive(bytes);
    }
  } catch (error) { run.error = { cause: error }; }
  return run;
}

const CONFLICT = Symbol("wallet store changed");
/** Records the SDK writes outside `snapshot:`; used when an adapter cannot list records. */
const KNOWN_RECORDS = [SAVED_WALLET_KEY, GENERATION_KEY, "meta", "seed.enc", "passkey.v1", "passkey.vault.v1",
  "vault:seed:generation", "vault:passkey:generation"];

/** A new encrypted store starts only in an empty one, never on top of unencrypted records. */
function whenEmpty(tx: WalletStorageTransaction<void>, next: () => void): void {
  const refuse = () => tx.fail(notEncrypted());
  if (tx.keys) { tx.keys(names => names.length ? refuse() : next()); return; }
  let pending = KNOWN_RECORDS.length, found = false;
  for (const name of KNOWN_RECORDS) tx.has(name, exists => {
    found ||= exists;
    if (--pending === 0) { if (found) refuse(); else next(); }
  });
}

async function commit(inner: WalletStorage, key: CryptoKey, meta: Meta, ops: Op[],
  options: { signal?: AbortSignal; commitWinsCancellation?: boolean }): Promise<boolean> {
  const fresh = !meta.head;
  const storeId = meta.storeId ?? crypto.randomUUID();
  const seq = (meta.head?.seq ?? 0) + 1;
  const keys: Record<string, number> = { ...meta.head?.keys };
  let rewriteMarker = fresh;
  for (const op of ops) {
    if ("put" in op) { if (!writeOnce(op.put)) keys[op.put] = seq; }
    else if ("remove" in op) delete keys[op.remove];
    else {
      for (const name of Object.keys(keys)) if (name.startsWith(op.prefix)) delete keys[name];
      if (MARKER_KEY.startsWith(op.prefix)) rewriteMarker = true;
    }
  }
  const sealed = await Promise.all(ops.map(op => "put" in op ? seal(key, storeId, op.put, seq, op.bytes) : undefined));
  const head = await seal(key, storeId, HEAD_KEY, seq, encodeValue({ seq, keys }));
  const marker = rewriteMarker ? fresh ? await newMarker(key, storeId) : meta.markerRaw : undefined;
  try {
    await inner.transaction<void>("readwrite", tx => {
      let markerNow: unknown, headNow: unknown, pending = 2;
      const apply = () => {
        if (--pending) return;
        // Every encrypted commit rewrites the head with a fresh nonce, so an
        // unchanged head proves that nothing this run read has changed.
        if (!sameRaw(markerNow, meta.markerRaw) || !sameRaw(headNow, meta.headRaw)) { tx.fail(CONFLICT); return; }
        if (fresh) whenEmpty(tx, write);
        else write();
      };
      const write = () => {
        ops.forEach((op, i) => {
          if ("put" in op) tx.put(op.put, sealed[i]);
          else if ("remove" in op) tx.delete(op.remove);
          else tx.deletePrefix(op.prefix);
        });
        if (marker) tx.put(MARKER_KEY, marker);
        tx.put(HEAD_KEY, head);
      };
      tx.get(MARKER_KEY, value => { markerNow = value; apply(); });
      tx.get(HEAD_KEY, value => { headNow = value; apply(); });
    }, options);
    return true;
  } catch (error) {
    if (error === CONFLICT) return false;
    throw error;
  }
}

/**
 * Wrap a store so every record is encrypted. Same transaction contract: a
 * transaction's reads come from one committed state and its writes commit
 * atomically. A wrong key, a missing key or tampering fails with a typed
 * {@link WalletError}; it is never read as an empty wallet. Internal: hosts
 * pass `storageKey` to `createWallet`.
 */
export function encryptedWalletStorage(inner: WalletStorage, storeKey: WalletStoreKey): WalletStorage {
  const imported = importStoreKey(storeKey);
  imported.catch(() => {});
  const storage: WalletStorage & { [ENCRYPTED]: true } = {
    [ENCRYPTED]: true,
    get available() { return inner.available && !!globalThis.crypto?.subtle; },
    async transaction<T>(mode: "readonly" | "readwrite", body: (tx: WalletStorageTransaction<T>) => void,
      options: { signal?: AbortSignal; commitWinsCancellation?: boolean } = {}): Promise<T> {
      const { signal } = options;
      signal?.throwIfAborted();
      const key = await imported;
      let meta: Meta | undefined;
      let plain = new Map<string, Uint8Array | undefined>();
      let needed: string[] = [];
      for (let round = 0; round < MAX_ROUNDS; round++) {
        if (!meta || needed.length) {
          const fetched = await fetchRecords(inner, needed, signal);
          if (!meta || !sameRaw(fetched.markerRaw, meta.markerRaw) || !sameRaw(fetched.headRaw, meta.headRaw)) {
            // Another commit landed: discard everything read under the old head.
            meta = await readMeta(key, fetched.markerRaw, fetched.headRaw);
            plain = new Map();
          }
          const current = meta;
          const records = await Promise.all(needed.map(name => readRecord(key, current, name, fetched.values.get(name))));
          needed.forEach((name, i) => plain.set(name, records[i]));
        }
        signal?.throwIfAborted();
        const run = dryRun(mode, body, plain, signal);
        if (run.error) throw run.error.cause;
        if (run.misses.size) { needed = [...run.misses]; continue; }
        if (mode === "readonly" || !run.ops.length) return run.value as T;
        signal?.throwIfAborted();
        if (await commit(inner, key, meta, run.ops, options)) return run.value as T;
        meta = undefined;
        plain = new Map();
        needed = [];
      }
      throw changing();
    },
  };
  return storage;
}

export function isEncryptedWalletStorage(storage: WalletStorage): boolean {
  return ENCRYPTED in storage;
}

/**
 * The unencrypted path, unchanged on disk, that refuses an encrypted store
 * instead of reading it as empty: reads fail on encrypted records, and writes
 * first check for the encryption marker.
 */
export function plainWalletStorage(inner: WalletStorage): WalletStorage {
  return {
    get available() { return inner.available; },
    transaction<T>(mode: "readonly" | "readwrite", body: (tx: WalletStorageTransaction<T>) => void,
      options?: { signal?: AbortSignal; commitWinsCancellation?: boolean }): Promise<T> {
      return inner.transaction<T>(mode, tx => {
        const guarded: WalletStorageTransaction<T> = {
          ...tx,
          get: (key, receive) => tx.get(key, value => { if (branded(value)) throw locked(); receive(value); }),
          has: (key, receive) => tx.has(key, receive),
          put: (key, value) => tx.put(key, value),
          delete: key => tx.delete(key),
          deletePrefix: prefix => tx.deletePrefix(prefix),
          result: value => tx.result(value),
          fail: error => tx.fail(error),
        };
        if (mode === "readonly") { body(guarded); return; }
        tx.get(MARKER_KEY, marker => { if (marker !== undefined) throw locked(); body(guarded); });
      }, options);
    },
  };
}

type StoreOptions = { storage?: WalletStorage; signal?: AbortSignal };

/** Inspect a store without its key, for example to choose an unlock or create screen. */
export async function inspectWalletStore(options: StoreOptions = {}): Promise<WalletStoreStatus> {
  const inner = options.storage ?? indexedDbWalletStorage();
  if (!inner.available) return { exists: false, encrypted: false };
  return inner.transaction<WalletStoreStatus>("readonly", tx => {
    tx.get(MARKER_KEY, marker => tx.get(SAVED_WALLET_KEY, saved => {
      tx.result({ exists: saved !== undefined && saved !== null, encrypted: marker !== undefined || branded(saved) });
    }));
  }, { signal: options.signal });
}

type Contents = { names: string[]; values: Map<string, unknown> };

function readAll(inner: WalletStorage, signal?: AbortSignal): Promise<Contents> {
  return inner.transaction<Contents>("readonly", tx => {
    if (!tx.keys) throw new TypeError("This WalletStorage adapter cannot list records, so it cannot be migrated.");
    tx.keys(names => {
      const contents: Contents = { names, values: new Map() };
      let pending = names.length;
      if (!pending) { tx.result(contents); return; }
      for (const name of names) tx.get(name, value => { contents.values.set(name, value); if (--pending === 0) tx.result(contents); });
    });
  }, { signal });
}

function sameNames(a: string[], b: string[]): boolean {
  const set = new Set(a);
  return a.length === b.length && b.every(name => set.has(name));
}

function isBlob(value: unknown): value is Blob { return typeof Blob !== "undefined" && value instanceof Blob; }

/** Replace the whole store in one atomic transaction if it still holds what was read. */
async function replaceAll(inner: WalletStorage, before: Contents, unchanged: (name: string, now: unknown) => boolean,
  write: (tx: WalletStorageTransaction<void>) => void, signal?: AbortSignal): Promise<boolean> {
  try {
    await inner.transaction<void>("readwrite", tx => {
      tx.keys!(names => {
        if (!sameNames(names, before.names)) { tx.fail(CONFLICT); return; }
        let pending = names.length;
        if (!pending) { write(tx); return; }
        for (const name of names) tx.get(name, now => {
          if (!unchanged(name, now)) { tx.fail(CONFLICT); return; }
          if (--pending === 0) write(tx);
        });
      });
    }, { signal });
    return true;
  } catch (error) {
    if (error === CONFLICT) return false;
    throw error;
  }
}

/**
 * Encrypt an existing unencrypted store in place, in one atomic transaction:
 * an interruption leaves the complete unencrypted store, and a retry starts
 * again. No unencrypted record remains in the store afterwards. Run it while
 * no wallet client is open. Calling it on a store already encrypted with this
 * key does nothing.
 */
export async function encryptWalletStore(options: StoreOptions & { key: WalletStoreKey }): Promise<void> {
  const inner = options.storage ?? indexedDbWalletStorage();
  const key = await importStoreKey(options.key);
  for (let attempt = 0; attempt < MAX_ROUNDS; attempt++) {
    const before = await readAll(inner, options.signal);
    if (before.values.has(MARKER_KEY)) {
      await readMeta(key, before.values.get(MARKER_KEY), before.values.get(HEAD_KEY));
      return;
    }
    const normalized = new Map<string, unknown>();
    for (const name of before.names) {
      const value = before.values.get(name);
      if (branded(value) || reserved(name)) throw corrupt("partly encrypted store");
      // Legacy snapshot payloads may be Blobs; they are read back as bytes.
      normalized.set(name, isBlob(value) ? new Uint8Array(await value.arrayBuffer()) : value);
    }
    const storeId = crypto.randomUUID();
    const keys: Record<string, number> = {};
    for (const name of before.names) if (!writeOnce(name)) keys[name] = 1;
    const sealed = await Promise.all(before.names.map(name => seal(key, storeId, name, 1, encodeValue(normalized.get(name)))));
    const marker = await newMarker(key, storeId);
    const head = await seal(key, storeId, HEAD_KEY, 1, encodeValue({ seq: 1, keys }));
    const encoded = new Map(before.names.map(name => [name, isBlob(before.values.get(name)) ? null : encodeValue(normalized.get(name))]));
    const done = await replaceAll(inner, before, (name, now) => {
      const was = before.values.get(name);
      if (isBlob(was)) return isBlob(now) && now.size === was.size;
      return !isBlob(now) && !branded(now) && sameBytes(encodeValue(now), encoded.get(name)!);
    }, tx => {
      before.names.forEach((name, i) => tx.put(name, sealed[i]));
      tx.put(MARKER_KEY, marker);
      tx.put(HEAD_KEY, head);
    }, options.signal);
    if (done) return;
  }
  throw changing();
}

/**
 * Decrypt an encrypted store back to the unencrypted format in place, in one
 * atomic transaction: an interruption leaves the complete encrypted store.
 * Every record is verified first. Run it while no wallet client is open.
 * Calling it on an unencrypted store does nothing.
 */
export async function decryptWalletStore(options: StoreOptions & { key: WalletStoreKey }): Promise<void> {
  const inner = options.storage ?? indexedDbWalletStorage();
  const key = await importStoreKey(options.key);
  for (let attempt = 0; attempt < MAX_ROUNDS; attempt++) {
    const before = await readAll(inner, options.signal);
    if (!before.values.has(MARKER_KEY)) {
      if (before.names.some(name => branded(before.values.get(name)))) throw corrupt("missing marker");
      return;
    }
    const meta = await readMeta(key, before.values.get(MARKER_KEY), before.values.get(HEAD_KEY));
    const names = before.names.filter(name => !reserved(name));
    for (const name of Object.keys(meta.head!.keys)) if (!before.values.has(name)) throw corrupt("missing record");
    const plain = await Promise.all(names.map(async name => decodeValue((await readRecord(key, meta, name, before.values.get(name)))!)));
    const done = await replaceAll(inner, before, (name, now) => reserved(name) ? sameRaw(now, before.values.get(name)) : true, tx => {
      names.forEach((name, i) => tx.put(name, plain[i]));
      tx.delete(HEAD_KEY);
      tx.delete(MARKER_KEY);
    }, options.signal);
    if (done) return;
  }
  throw changing();
}

/**
 * Delete every record of a store, encrypted or not, without its key: for a
 * host whose key is lost. The wallet must then be restored from its recovery
 * phrase. Leaves a fresh generation so other open pages stop using old state.
 */
export async function eraseWalletStore(options: StoreOptions = {}): Promise<void> {
  const inner = options.storage ?? indexedDbWalletStorage();
  await inner.transaction<void>("readwrite", tx => {
    tx.deletePrefix("");
    tx.put(GENERATION_KEY, crypto.randomUUID());
  }, { signal: options.signal });
}
