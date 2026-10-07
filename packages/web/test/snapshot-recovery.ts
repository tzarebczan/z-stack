import type { WalletSnapshot } from "@z-stack/core";
import { WalletError } from "@z-stack/core";
import { abortSnapshotWrites, openWalletDb, peekSavedSnapshot, readSavedSnapshot, readSavedSnapshotRecord, saveWalletSnapshot } from "../../sdk/src/snapshot-storage";

const marker = "z-stack.synthetic-recovery";
const size = 8_000_000;
const preview = (height: number) => ({ network: "regtest", server: "", birthdayHeight: 1,
  scannedHeight: height, unifiedAddress: marker, transparentAddress: null, zip321: "", viewOnly: true,
  balance: { totalAvailable: 0, orchardAvailable: 0 } } as WalletSnapshot);
function check(value: unknown, message: string): asserts value { if (!value) throw new Error(message); }
const checksum = async (bytes: Uint8Array<ArrayBuffer>) => Array.from(
  new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)), n => n.toString(16).padStart(2, "0"),
).join("");

async function prepare() {
  check(location.hostname === "127.0.0.1" && location.port === "15182", "Use isolated port 15182.");
  const db = await openWalletDb();
  try {
    const keys = await new Promise<IDBValidKey[]>((resolve, reject) => {
      const tx = db.transaction("wallets", "readonly"), req = tx.objectStore("wallets").getAllKeys();
      tx.oncomplete = () => resolve(req.result); tx.onabort = () => reject(tx.error);
    });
    check(keys.every(key => key === "wallet:generation"), "Existing records refused.");
  } finally { db.close(); }
  const bytes = new Uint8Array(size);
  for (let at = 0; at < size; at += 65_536) crypto.getRandomValues(bytes.subarray(at, Math.min(at + 65_536, size)));
  await saveWalletSnapshot(bytes, preview(10), () => true);
  return checksum(bytes);
}
async function verify() {
  const saved = await peekSavedSnapshot(() => null);
  check(saved?.unifiedAddress === marker && saved.scannedHeight === 10, "Last committed preview must survive.");
  const bytes = await readSavedSnapshot();
  check(bytes?.length === size, "Last committed payload must survive.");
  return checksum(new Uint8Array(bytes));
}
async function replacement(fault: "quota" | "cancel" | "terminate") {
  check(location.port === "15182", "Wrong test origin.");
  await verify();
  const record = await readSavedSnapshotRecord();
  check(record?.bytes, "Missing fixture.");
  const bytes = record.bytes.slice();
  for (let at = 0; at < bytes.length; at += 8192) bytes[at] ^= 255;
  const original = IDBObjectStore.prototype.put;
  let writes = 0;
  IDBObjectStore.prototype.put = function(value: unknown, key?: IDBValidKey) {
    if (value instanceof Uint8Array && ++writes === 2) {
      if (fault === "quota") throw new DOMException("Injected storage pressure", "QuotaExceededError");
      if (fault === "cancel") queueMicrotask(abortSnapshotWrites);
    }
    const result = original.call(this, value, key);
    if (fault === "terminate" && key === "default") {
      // Keep a real IDB write transaction alive after its manifest is queued.
      // Closing/crashing the page must roll it back as a unit.
      const store = this;
      const hold = () => { const request = store.get("default"); request.onsuccess = hold; };
      hold();
      document.body.dataset.held = "true";
    }
    return result;
  };
  try {
    await saveWalletSnapshot(bytes, preview(20), () => true, record.generation, record.key);
    throw new Error("Fault did not interrupt the write.");
  } catch (error) {
    if (fault === "quota") check(error instanceof WalletError && error.code === "storage_full", "Quota error must have actionable copy.");
    else if (fault === "cancel") check(error instanceof DOMException && error.name === "AbortError", "Cancellation must abort the transaction.");
    else throw error;
  } finally { IDBObjectStore.prototype.put = original; }
  return verify();
}
Object.assign(window, { snapshotRecovery: { prepare, verify, replacement } });
