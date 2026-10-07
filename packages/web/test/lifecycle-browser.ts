import type { WalletSnapshot } from "@z-stack/core";
import { forgetWasmWallet } from "../../sdk/src/wasm-client";
import { hasEncryptedSeed, persistEncryptedSeed, unlockEncryptedSeed } from "../../sdk/src/seed-vault";
import { hasPasskeySeed } from "../../sdk/src/passkey";
import { advanceWalletGeneration, openWalletDb, readSavedSnapshot, readSavedSnapshotRecord, saveWalletSnapshot } from "../../sdk/src/snapshot-storage";

const output = document.querySelector<HTMLPreElement>("#results")!;
const button = document.querySelector<HTMLButtonElement>("#run")!;
const fixture = "synthetic non-wallet lifecycle fixture";
const passphrase = "synthetic browser test passphrase";
const preview: WalletSnapshot = {
  network: "regtest", server: "", birthdayHeight: 1, scannedHeight: 10,
  unifiedAddress: "lifecycle-test-fixture", transparentAddress: null, zip321: "",
  balance: { saplingAvailable: 0, orchardAvailable: 0, ironwoodAvailable: 0,
    transparentAvailable: 0, totalAvailable: 0, saplingZec: "0", orchardZec: "0",
    ironwoodZec: "0", transparentZec: "0", totalZec: "0" },
};
function check(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message);
}
async function keys(): Promise<IDBValidKey[]> {
  const db = await openWalletDb();
  try {
    return await new Promise((resolve, reject) => {
      const tx = db.transaction("wallets", "readonly");
      const request = tx.objectStore("wallets").getAllKeys();
      tx.oncomplete = () => resolve(request.result);
      tx.onabort = () => reject(tx.error ?? new Error("read aborted"));
    });
  } finally { db.close(); }
}
async function put(key: string, value: unknown): Promise<void> {
  const db = await openWalletDb();
  try {
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction("wallets", "readwrite");
      tx.objectStore("wallets").put(value, key);
      tx.oncomplete = () => resolve();
      tx.onabort = () => reject(tx.error ?? new Error("write aborted"));
    });
  } finally { db.close(); }
}
const walletKeys = async () => (await keys()).filter(key => !/^wallet:generation$|^vault:(seed|passkey):generation$/.test(String(key)));
const settle = (promise: Promise<unknown>) => promise.then(
  () => ({ ok: true, error: "" }), (error) => ({ ok: false, error: String(error) }),
);

if (new URLSearchParams(location.search).has("peer")) {
  button.hidden = true;
  let held: Awaited<ReturnType<typeof readSavedSnapshotRecord>>;
  window.addEventListener("message", async event => {
    if (event.origin !== location.origin || event.source !== parent || !event.data?.fixtureCommand) return;
    const { fixtureCommand, id } = event.data;
    const outcome = await settle((async () => {
      if (fixtureCommand === "capture") {
        held = await readSavedSnapshotRecord();
        check(held, "peer could not capture fixture");
      } else if (fixtureCommand === "save") {
        check(held, "peer has no captured fixture");
        await saveWalletSnapshot(held.bytes, preview, () => true, held.generation);
      } else if (fixtureCommand === "claim") {
        check(held, "peer has no captured fixture");
        await advanceWalletGeneration(undefined, held.generation);
      } else throw new Error("unknown fixture command");
    })());
    parent.postMessage({ fixtureReply: id, ...outcome }, location.origin);
  });
} else button.onclick = async () => {
  button.disabled = true;
  let ownsFixture = false;
  const results: Array<{ test: string; ms: number }> = [];
  const originalDelete = IDBObjectStore.prototype.delete;
  async function run(test: string, work: () => Promise<void>) {
    const start = performance.now();
    await work();
    results.push({ test, ms: +(performance.now() - start).toFixed(2) });
    output.textContent = JSON.stringify({ status: "running", results }, null, 2);
  }
  try {
    check(location.hostname === "127.0.0.1" && location.port === "15180",
      "Use isolated http://127.0.0.1:15180/test/lifecycle-browser.html");
    check((await walletKeys()).length === 0, "Existing records found; nothing changed. Use a fresh test origin.");
    ownsFixture = true;
    await run("real WebCrypto encrypted seed roundtrip", async () => {
      await persistEncryptedSeed(passphrase, fixture);
      check(await unlockEncryptedSeed(passphrase) === fixture, "seed roundtrip mismatch");
      check(!(await settle(unlockEncryptedSeed("wrong fixture passphrase"))).ok, "wrong passphrase accepted");
    });
    await saveWalletSnapshot(new Uint8Array([1, 2, 3]), preview, () => true);
    await put("meta", { unlockPolicy: "session" });
    await put("passkey.v1", { credId: "synthetic", mode: "prf", createdAt: 1 });
    await run("failed Forget rolls back snapshot, metadata and encrypted seed together", async () => {
      const before = JSON.stringify(await keys());
      IDBObjectStore.prototype.delete = function(key) {
        if (key === "seed.enc") throw new DOMException("injected deletion failure", "UnknownError");
        return originalDelete.call(this, key);
      };
      let result;
      try { result = await settle(forgetWasmWallet()); }
      finally { IDBObjectStore.prototype.delete = originalDelete; }
      check(!result.ok && result.error.includes("injected deletion failure"), "Forget hid the deletion error");
      check(JSON.stringify(await keys()) === before, "failed Forget partially deleted records");
      check((await readSavedSnapshot())?.[2] === 3, "snapshot lost after aborted Forget");
      check(await hasEncryptedSeed(), "encrypted seed lost after aborted Forget");
    });
    await run("Forget retains committed passkey by default; explicit removal deletes it", async () => {
      await forgetWasmWallet();
      check(!(await hasEncryptedSeed()) && !(await readSavedSnapshot()), "wallet records survived Forget");
      check(await hasPasskeySeed(), "default Forget removed the passkey");
      await forgetWasmWallet({ passkey: true });
      check((await walletKeys()).length === 0, "explicit passkey removal left records");
    });
    await run("encryption started before Forget cannot republish seed.enc", async () => {
      const pending = settle(persistEncryptedSeed(passphrase, fixture));
      await forgetWasmWallet();
      const result = await pending;
      check(!result.ok, "invalidated encryption reported success");
      check(!(await hasEncryptedSeed()), "encrypted seed reappeared after Forget");
    });
    await run("another document cannot save or claim its old wallet after Forget", async () => {
      await saveWalletSnapshot(new Uint8Array([1, 2, 3]), preview, () => true);
      const frame = document.createElement("iframe");
      frame.hidden = true;
      const loaded = new Promise<void>(resolve => { frame.onload = () => resolve(); });
      frame.src = `${location.pathname}?peer`;
      document.body.append(frame);
      try {
        await loaded;
        const command = (fixtureCommand: string) => new Promise<{ ok: boolean; error: string }>((resolve, reject) => {
          const id = crypto.randomUUID();
          const timer = setTimeout(() => { window.removeEventListener("message", receive); reject(new Error("peer timed out")); }, 5000);
          const receive = (event: MessageEvent) => {
            if (event.source !== frame.contentWindow || event.origin !== location.origin || event.data?.fixtureReply !== id) return;
            clearTimeout(timer);
            window.removeEventListener("message", receive);
            resolve(event.data);
          };
          window.addEventListener("message", receive);
          frame.contentWindow!.postMessage({ fixtureCommand, id }, location.origin);
        });
        check((await command("capture")).ok, "peer capture failed");
        await forgetWasmWallet();
        const saved = await command("save");
        check(!saved.ok && saved.error.includes("saved wallet changed"), "stale document republished a snapshot");
        const claimed = await command("claim");
        check(!claimed.ok && claimed.error.includes("saved wallet changed"), "stale document claimed a fresh identity");
        check(!(await readSavedSnapshot()), "forgotten wallet reappeared");
      } finally { frame.remove(); }
    });
    await run("a new write after Forget still works", async () => {
      await persistEncryptedSeed(passphrase, fixture);
      check(await unlockEncryptedSeed(passphrase) === fixture, "new operation remained cancelled");
      await forgetWasmWallet({ passkey: true });
      check((await walletKeys()).length === 0, "fixture cleanup failed");
    });
    ownsFixture = false;
    output.textContent = JSON.stringify({ status: "passed", userAgent: navigator.userAgent, results }, null, 2);
  } catch (error) {
    output.textContent = JSON.stringify({ status: "failed", message: String(error), results }, null, 2);
  } finally {
    IDBObjectStore.prototype.delete = originalDelete;
    if (ownsFixture) await forgetWasmWallet({ passkey: true });
    button.disabled = false;
  }
};
