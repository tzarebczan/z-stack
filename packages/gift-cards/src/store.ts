import { GiftError, decodeBase64Url, encodeBase64Url } from "./protocol";
import type { GiftRecord, GiftStore } from "./service";

/** Validate decrypted application records. Failures never include their secret fields. */
export function parseGiftRecord(raw: unknown): GiftRecord {
  if (typeof raw !== "object" || raw === null || !("id" in raw) || typeof raw.id !== "string" ||
    !/^(mainnet|testnet|regtest):[0-9a-f]{64}$/.test(raw.id) ||
    !("revision" in raw) || !Number.isSafeInteger(raw.revision) || typeof raw.revision !== "number" || raw.revision < 0 ||
    !("role" in raw) || (raw.role !== "issued" && raw.role !== "received") ||
    !("link" in raw) || typeof raw.link !== "string" || raw.link.length > 16_384 ||
    !("address" in raw) || typeof raw.address !== "string" || raw.address.length > 1024 ||
    !("amountZat" in raw) || typeof raw.amountZat !== "string" || !/^[1-9]\d{0,15}$/.test(raw.amountZat) ||
    !("phase" in raw)) throw new GiftError("storage");
  const base: Pick<GiftRecord, "id" | "revision" | "role" | "link" | "address" | "amountZat"> = { id: raw.id, revision: raw.revision, role: raw.role, link: raw.link, address: raw.address, amountZat: raw.amountZat };
  const fundingTxid = "fundingTxid" in raw && raw.fundingTxid !== undefined ? validTxid(raw.fundingTxid) : undefined;
  const feeZat = "feeZat" in raw && raw.feeZat !== undefined ? validFee(raw.feeZat) : undefined;
  switch (raw.phase) {
    case "draft":
      if (fundingTxid || feeZat || ("claimTxid" in raw && raw.claimTxid !== undefined) || ("destination" in raw && raw.destination !== undefined)) throw new GiftError("storage");
      return { ...base, phase: "draft" };
    case "funding": case "funded":
      if (("claimTxid" in raw && raw.claimTxid !== undefined) || ("destination" in raw && raw.destination !== undefined)) throw new GiftError("storage");
      return { ...base, phase: raw.phase, ...(fundingTxid ? { fundingTxid } : {}), ...(feeZat ? { feeZat } : {}) };
    case "redeeming": case "redeemed": {
      if (!("destination" in raw) || typeof raw.destination !== "string" || raw.destination.length > 1024 || !feeZat) throw new GiftError("storage");
      const claimTxid = "claimTxid" in raw && raw.claimTxid !== undefined ? validTxid(raw.claimTxid) : undefined;
      if (raw.phase === "redeemed") {
        if (!claimTxid) throw new GiftError("storage");
        return { ...base, phase: "redeemed", destination: raw.destination, feeZat, claimTxid, ...(fundingTxid ? { fundingTxid } : {}) };
      }
      return { ...base, phase: "redeeming", destination: raw.destination, feeZat, ...(claimTxid ? { claimTxid } : {}), ...(fundingTxid ? { fundingTxid } : {}) };
    }
    default: throw new GiftError("storage");
  }
}
function validTxid(value: unknown): string {
  if (typeof value !== "string" || !/^[0-9a-f]{64}$/i.test(value)) throw new GiftError("storage");
  return value.toLowerCase();
}
function validFee(value: unknown): string {
  if (typeof value !== "string" || !/^(0|[1-9]\d{0,15})$/.test(value)) throw new GiftError("storage");
  return value;
}
type Envelope = { revision: number; iv: string; ciphertext: string };
function envelope(raw: unknown): Envelope | null {
  if (raw === undefined) return null;
  if (typeof raw !== "object" || raw === null || !("revision" in raw) || typeof raw.revision !== "number" ||
    !Number.isSafeInteger(raw.revision) || raw.revision < 0 || !("iv" in raw) || typeof raw.iv !== "string" ||
    !("ciphertext" in raw) || typeof raw.ciphertext !== "string" || raw.ciphertext.length > 100_000) throw new GiftError("storage");
  return { revision: raw.revision, iv: raw.iv, ciphertext: raw.ciphertext };
}

/** Caller supplies an unlocked AES-GCM key. No phrase, passkey, account or backend is required. */
export function createEncryptedGiftStore(options: { name: string; key: CryptoKey; factory?: IDBFactory }): GiftStore {
  if (!options.name.trim() || options.key.algorithm.name !== "AES-GCM" || !options.key.usages.includes("encrypt") || !options.key.usages.includes("decrypt")) throw new GiftError("storage");
  const open = async (): Promise<IDBDatabase> => new Promise((resolve, reject) => {
    const request = (options.factory ?? indexedDB).open(options.name, 1);
    request.onupgradeneeded = () => request.result.createObjectStore("gifts");
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(new GiftError("storage"));
  });
  const decode = async (id: string, stored: Envelope): Promise<GiftRecord> => {
    try {
      const iv = decodeBase64Url(stored.iv);
      if (iv.length !== 12) throw new GiftError("storage");
      const bytes = await crypto.subtle.decrypt({ name: "AES-GCM", iv: Uint8Array.from(iv), additionalData: new TextEncoder().encode(`${options.name}:${id}:${stored.revision}`) },
        options.key, Uint8Array.from(decodeBase64Url(stored.ciphertext, 100_000)));
      const record = parseGiftRecord(JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)));
      if (record.id !== id || record.revision !== stored.revision) throw new GiftError("storage");
      return record;
    } catch { throw new GiftError("storage"); }
  };
  const get = async (id: string) => {
    const db = await open();
    try {
      const stored = await new Promise<Envelope | null>((resolve, reject) => {
        const request = db.transaction("gifts", "readonly").objectStore("gifts").get(id);
        request.onsuccess = () => { try { resolve(envelope(request.result)); } catch { reject(new GiftError("storage")); } };
        request.onerror = () => reject(new GiftError("storage"));
      });
      return stored ? await decode(id, stored) : null;
    } finally { db.close(); }
  };
  return {
    get,
    async list() {
      const db = await open();
      try {
        const ids = await new Promise<string[]>((resolve, reject) => {
          const req = db.transaction("gifts", "readonly").objectStore("gifts").getAllKeys();
          req.onsuccess = () => { if (req.result.every(key => typeof key === "string")) resolve(req.result); else reject(new GiftError("storage")); };
          req.onerror = () => reject(new GiftError("storage"));
        });
        const records = await Promise.all(ids.map(get));
        return records.filter(record => record !== null);
      } finally { db.close(); }
    },
    async save(record, expectedRevision) {
      parseGiftRecord(record);
      if (record.revision !== (expectedRevision === null ? 0 : expectedRevision + 1)) throw new GiftError("conflict");
      const iv = crypto.getRandomValues(new Uint8Array(12));
      const ciphertext = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: new TextEncoder().encode(`${options.name}:${record.id}:${record.revision}`) },
        options.key, new TextEncoder().encode(JSON.stringify(record)));
      const stored: Envelope = { revision: record.revision, iv: encodeBase64Url(iv), ciphertext: encodeBase64Url(new Uint8Array(ciphertext)) };
      const db = await open();
      try {
        await new Promise<void>((resolve, reject) => {
          const tx = db.transaction("gifts", "readwrite"), data = tx.objectStore("gifts");
          let conflict = false;
          const read = data.get(record.id);
          read.onsuccess = () => {
            try {
              const prior = envelope(read.result);
              if ((prior?.revision ?? null) !== expectedRevision) { conflict = true; tx.abort(); }
              else data.put(stored, record.id);
            } catch { tx.abort(); }
          };
          tx.oncomplete = () => resolve();
          tx.onabort = tx.onerror = () => reject(new GiftError(conflict ? "conflict" : "storage"));
        });
      } finally { db.close(); }
    },
    async lock(id, body) {
      if (typeof navigator !== "undefined" && navigator.locks) return navigator.locks.request(`z-stack:gift-record:${options.name}:${id}`, body);
      // CAS still makes funding/redeeming transitions exclusive. Browser engine adapter requires its own lease.
      return body();
    },
  };
}
