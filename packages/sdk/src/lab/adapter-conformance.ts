/** Adapter acceptance helpers. Run only against an isolated, disposable store. */
import type { WalletStorage } from "../storage";
import type { PasskeyVaultRecord, PasskeyVaultStore } from "@z-stack/passkey";
import { PasskeyError } from "@z-stack/passkey";

export class AdapterConformanceError extends Error {
  constructor(readonly check: string) { super(`Adapter contract failed: ${check}`); this.name = "AdapterConformanceError"; }
}
export interface AdapterTestReport { passed: string[]; untested: string[] }
/** Every open must connect to the same disposable namespace, including after reopen. */
export interface AdapterTestFixture<Store> {
  open(): Store | Promise<Store>;
  dispose(): void | Promise<void>;
  /** Optional fixture fault: reject the next write at commit (e.g. quota/disk failure). */
  failNextCommit?: () => void | Promise<void>;
}
const requireContract = (valid: boolean, check: string) => { if (!valid) throw new AdapterConformanceError(check); };
async function mustReject(work: Promise<unknown>, check: string, predicate = (_error: unknown) => true) {
  try { await work; } catch (error) { requireContract(predicate(error), check); return; }
  throw new AdapterConformanceError(check);
}
const read = (store: WalletStorage, key: string) => store.transaction<unknown>("readonly", tx => tx.get(key, value => tx.result(value)));

/** Tests queue semantics, isolation, failure atomicity, cloning, cancellation, CAS and reopen. */
export async function checkWalletStorageAdapter(fixture: AdapterTestFixture<WalletStorage>): Promise<AdapterTestReport> {
  const passed: string[] = [];
  const untested = ["process crash/power loss", "physical commit winning cancellation", "browser eviction"];
  try {
    const store = await fixture.open();
    const peer = await fixture.open();
    requireContract(store.available && peer.available, "available");
    const original = { count: 1 };
    await store.transaction("readwrite", tx => { tx.put("snapshot:a", original); tx.put("snapshot:b", { count: 1 }); });
    original.count = 999;
    const copy = await read(peer, "snapshot:a") as { count: number };
    requireContract(copy?.count === 1, "clone-on-write");
    copy.count = 888;
    requireContract((await read(store, "snapshot:a") as { count: number }).count === 1, "clone-on-read");
    passed.push("cloned values and cross-connection visibility");

    await mustReject(store.transaction("readwrite", tx => {
      tx.put("snapshot:a", { count: 2 }); tx.delete("snapshot:b"); tx.fail(new Error("fixture rollback"));
    }), "failed-transaction-rejects");
    requireContract((await read(peer, "snapshot:a") as { count: number }).count === 1 &&
      (await read(peer, "snapshot:b") as { count: number }).count === 1, "failure-atomicity");
    passed.push("failed multi-record write preserves prior state");

    const cancelled = new AbortController(); cancelled.abort();
    let called = false;
    await mustReject(store.transaction("readwrite", tx => { called = true; tx.put("cancelled", true); },
      { signal: cancelled.signal }), "pre-aborted-transaction");
    requireContract(!called && await read(peer, "cancelled") === undefined, "pre-aborted-body");
    const during = new AbortController();
    await mustReject(store.transaction("readwrite", tx => {
      tx.put("snapshot:a", { count: 3 }); during.abort();
    }, { signal: during.signal }), "abort-before-commit");
    requireContract((await read(peer, "snapshot:a") as { count: number }).count === 1, "abort-atomicity");
    passed.push("pre-commit cancellation preserves prior state");

    await store.transaction("readwrite", tx => tx.put("counter", 0));
    await Promise.all(Array.from({ length: 24 }, (_, index) => (index % 2 ? peer : store).transaction("readwrite", tx => {
      tx.get("counter", value => tx.put("counter", Number(value) + 1));
    })));
    requireContract(await read(store, "counter") === 24, "serialized-cross-connection-transactions");
    const cas = (connection: WalletStorage) => connection.transaction("readwrite", tx => tx.get("counter", value => {
      if (value !== 24) tx.fail(new Error("fixture conflict"));
      else tx.put("counter", 25);
    }));
    const writes = await Promise.allSettled([cas(store), cas(peer)]);
    requireContract(writes.filter(result => result.status === "fulfilled").length === 1 && await read(store, "counter") === 25, "atomic-CAS");
    passed.push("serialized read callbacks and atomic compare-and-swap");

    await store.transaction("readwrite", tx => { tx.put("generation", "forgotten"); tx.deletePrefix("snapshot:"); });
    const reopened = await fixture.open();
    const tombstone = await reopened.transaction<{ exists: boolean; generation: unknown }>("readonly", tx => {
      tx.has("snapshot:a", exists => tx.get("generation", generation => tx.result({ exists, generation })));
    });
    requireContract(!tombstone.exists && await read(reopened, "snapshot:b") === undefined && tombstone.generation === "forgotten", "prefix-delete-and-reopen-tombstone");
    passed.push("prefix deletion preserves generation across reopen");
    await mustReject(store.transaction("readonly", tx => tx.put("readonly-write", true)), "readonly-write-rejected");
    requireContract(await read(peer, "readonly-write") === undefined, "readonly-write-atomicity");
    passed.push("readonly transaction cannot mutate records");
    if (fixture.failNextCommit) {
      await fixture.failNextCommit();
      await mustReject(store.transaction("readwrite", tx => { tx.put("counter", 100); tx.put("quota-extra", true); }), "commit-failure-rejected");
      requireContract(await read(peer, "counter") === 25 && await read(peer, "quota-extra") === undefined, "commit-failure-atomicity");
      passed.push("injected quota/disk commit failure preserves all prior records");
    } else untested.push("quota/disk commit failure (provide failNextCommit)");
    return { passed, untested };
  } catch (error) {
    if (error instanceof AdapterConformanceError) throw error;
    throw new AdapterConformanceError("adapter-operation");
  } finally {
    try { await fixture.dispose(); }
    catch { throw new AdapterConformanceError("fixture-dispose"); }
  }
}

const record = (revision: number): PasskeyVaultRecord => ({
  format: "z-stack/passkey-vault", version: 1, id: "conformance-vault", rpId: "localhost", purpose: "test-only",
  userId: "Zml4dHVyZQ", userName: "Fixture", revision, createdAt: 1, updatedAt: revision,
  data: { iv: "AAAAAAAAAAAAAAAA", ct: "AAAAAAAAAAAAAAAAAAAAAA" }, passkeys: [{
    credentialId: "Zml4dHVyZQ", salt: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    wrapped: { iv: "AAAAAAAAAAAAAAAA", ct: "AAAAAAAAAAAAAAAAAAAAAA" }, createdAt: 1,
  }],
});
/** Store protocol checks, not cryptographic acceptance of the synthetic test records. */
export async function checkVaultStoreAdapter(fixture: AdapterTestFixture<PasskeyVaultStore>): Promise<AdapterTestReport> {
  const passed: string[] = [];
  const untested = ["authentication/authorization", "server rollback", "process crash/power loss", "in-flight network cancellation"];
  try {
    const store = await fixture.open(); const peer = await fixture.open();
    requireContract(await store.get("conformance-vault") === undefined, "missing-vault");
    const initial = record(1);
    await store.put(initial, undefined); initial.userName = "changed";
    const saved = (await peer.get(initial.id))!;
    requireContract(saved.userName === "Fixture", "vault-clone-on-write");
    saved.userName = "changed";
    requireContract((await store.get(initial.id))?.userName === "Fixture", "vault-clone-on-read");
    passed.push("missing records, cloning and cross-connection reads");
    const conflict = (error: unknown) => error instanceof PasskeyError && error.code === "conflict";
    await mustReject(peer.put(record(2), undefined), "create-only-rejects-overwrite", conflict);
    const writes = await Promise.allSettled([store.put(record(2), 1), peer.put(record(2), 1)]);
    requireContract(writes.filter(result => result.status === "fulfilled").length === 1 &&
      writes.some(result => result.status === "rejected" && conflict(result.reason)), "vault-atomic-CAS");
    passed.push("create-only writes and concurrent revision conflict");
    const cancelled = new AbortController(); cancelled.abort();
    await mustReject(store.put(record(3), 2, cancelled.signal), "vault-pre-aborted-write");
    await mustReject(store.delete(initial.id, cancelled.signal), "vault-pre-aborted-delete");
    await mustReject(store.get(initial.id, cancelled.signal), "vault-pre-aborted-read");
    requireContract((await peer.get(initial.id))?.revision === 2, "vault-abort-preserves-record");
    passed.push("pre-aborted operations preserve records");
    const forgotten: PasskeyVaultRecord = { ...record(3), forgotten: true, userId: "", userName: "", rpId: "", data: { iv: "", ct: "" }, passkeys: [] };
    await store.put(forgotten, 2);
    const reopened = await fixture.open();
    requireContract((await reopened.get(initial.id))?.forgotten === true, "vault-tombstone-reopen");
    await mustReject(peer.put(record(2), 2), "vault-stale-write-after-forget", conflict);
    passed.push("durable tombstone rejects stale resurrection");
    if (fixture.failNextCommit) {
      await fixture.failNextCommit();
      await mustReject(store.put(record(4), 3), "vault-commit-failure");
      requireContract((await peer.get(initial.id))?.forgotten === true, "vault-failure-preserves-tombstone");
      passed.push("injected commit failure preserves previous generation");
    } else untested.push("quota/disk commit failure (provide failNextCommit)");
    await store.delete(initial.id);
    requireContract(await peer.get(initial.id) === undefined, "administrative-delete");
    passed.push("administrative hard-delete is separate from forget");
    return { passed, untested };
  } catch (error) {
    if (error instanceof AdapterConformanceError) throw error;
    throw new AdapterConformanceError("adapter-operation");
  } finally {
    try { await fixture.dispose(); }
    catch { throw new AdapterConformanceError("fixture-dispose"); }
  }
}
