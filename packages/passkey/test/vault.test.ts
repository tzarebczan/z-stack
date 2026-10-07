import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import {
  createPasskeyVault,
  getPasskeySupport,
  indexedDbVaultStore,
  isPasskeyCancel,
  memoryVaultStore,
  parseAuthenticatorData,
  PasskeyError,
  registrationJson,
  resolveRpId,
  type PasskeyVaultRecord,
  type PasskeyVaultStore,
} from "../src/index.ts";
import { fakeWebAuthn, PROVIDERS } from "./fake-authenticator.ts";
import { memoryIndexedDb } from "./idb-fixture.ts";

const WORDS = "legal winner thank year wave sausage worth useful legal winner thank yellow";
const RP = "wallet.example";

let fake: ReturnType<typeof fakeWebAuthn>;
let store: PasskeyVaultStore;

beforeEach(() => {
  fake = fakeWebAuthn();
  store = memoryVaultStore();
});
afterEach(() => fake.uninstall());

function vault(extra: Parameters<typeof createPasskeyVault>[0] extends infer O ? Partial<O> : never = {}) {
  return createPasskeyVault({ rpName: "Test Wallet", rpId: RP, store, ...extra });
}

async function code(p: Promise<unknown>): Promise<string> {
  try {
    await p;
  } catch (e) {
    assert.ok(e instanceof PasskeyError, `expected PasskeyError, got ${e}`);
    return e.code;
  }
  assert.fail("expected a rejection");
}

test("protect then unlock: one prompt each when PRF comes back at creation", async () => {
  const v = vault();
  const { passkey, response } = await v.protect(WORDS, { name: "Laptop" });
  assert.deepEqual(fake.log, ["create:gpm"]);
  assert.equal(passkey.provider, "Google Password Manager");
  assert.equal(passkey.synced, true);
  assert.equal(passkey.name, "Laptop");
  assert.equal(response.id, passkey.credentialId);

  const opened = await v.unlock();
  assert.deepEqual(fake.log, ["create:gpm", "get"]);
  assert.equal(opened.text(), WORDS);
  assert.equal(opened.restored, false);
  assert.equal(opened.passkey.credentialId, passkey.credentialId);
  assert.ok((await v.passkeys())[0]!.lastUsedAt);
});

test("PRF only on assertion (security keys): protect adds one follow-up prompt", async () => {
  fake.use(PROVIDERS.yubikey);
  const v = vault();
  const { passkey } = await v.protect(WORDS);
  assert.deepEqual(fake.log, ["create:yubikey", "get"]);
  assert.equal(passkey.synced, false, "a device-bound key is reported as not synced");
  assert.equal((await v.unlock()).text(), WORDS);
});

test("no PRF: prf-unsupported, nothing saved, and the orphan is signalled to the provider", async () => {
  fake.use(PROVIDERS.windowsHelloNoPrf);
  const v = vault();
  assert.equal(await code(v.protect(WORDS)), "prf-unsupported");
  assert.equal(await v.exists(), false);
  assert.equal(fake.signalled.length, 1);
  assert.deepEqual(fake.signalled[0], {
    kind: "unknown",
    arg: { rpId: RP, credentialId: Buffer.from(fake.credentials[0]!.id).toString("base64url") },
  });
});

test("early non-PRF rejection is observed while IndexedDB is still reading", async () => {
  fake.use(PROVIDERS.windowsHelloNoPrf);
  const backing = store;
  const delayed: PasskeyVaultStore = {
    ...backing,
    async get(id, signal) { await new Promise(resolve => setTimeout(resolve, 20)); return backing.get(id, signal); },
  };
  const v = vault({ store: delayed });
  assert.equal(await code(v.protect(WORDS)), "prf-unsupported");
  assert.equal(await backing.get("default"), undefined);
  assert.equal(await v.exists(), false);
});

test("cancelling the follow-up PRF prompt also retires the new credential", async () => {
  fake.use(PROVIDERS.yubikey);
  const v = vault();
  const create = fake.cancelNext.bind(fake);
  // Let create() succeed, then cancel the PRF assertion.
  const pending = v.protect(WORDS);
  create("get");
  const err = await pending.catch((e) => e);
  assert.ok(isPasskeyCancel(err));
  assert.equal(fake.signalled.length, 1);
  assert.equal(await v.exists(), false);
});

test("the record and the server JSON never contain the secret or the PRF output", async () => {
  const v = vault();
  const { response } = await v.protect(WORDS);
  const record = JSON.stringify(await v.export());
  for (const word of new Set(WORDS.split(" "))) assert.ok(!record.includes(word), `record leaks "${word}"`);
  const opened = await v.unlock();
  for (const json of [response, opened.response]) {
    const prf = (json.clientExtensionResults as { prf?: Record<string, unknown> }).prf;
    assert.ok(!prf || !("results" in prf), "PRF results must be stripped");
  }
});

test("registrationJson strips PRF results that toJSON() would include", () => {
  const credential = {
    rawId: new Uint8Array([1, 2]).buffer,
    authenticatorAttachment: "platform",
    response: { clientDataJSON: new ArrayBuffer(1), attestationObject: new ArrayBuffer(1) },
    getClientExtensionResults: () => ({ prf: { enabled: true, results: { first: new ArrayBuffer(32) } }, credProps: { rk: true } }),
  } as unknown as PublicKeyCredential;
  const json = registrationJson(credential);
  assert.deepEqual(json.clientExtensionResults, { prf: { enabled: true }, credProps: { rk: true } });
});

test("addPasskey: a second provider opens the same secret; same provider is excluded", async () => {
  const v = vault();
  const first = await v.protect(WORDS);
  assert.equal(await code(v.addPasskey()), "already-registered", "same provider must hit excludeCredentials");

  fake.log.length = 0;
  fake.use(PROVIDERS.yubikey);
  // Unlock with the GPM passkey, then register the YubiKey.
  fake.choose((ids) => (ids.includes(first.passkey.credentialId) ? first.passkey.credentialId : ids[0]));
  const second = await v.addPasskey({ name: "YubiKey" });
  fake.choose(null);
  assert.deepEqual(fake.log, ["get", "create:yubikey", "get"]);
  assert.equal((await v.passkeys()).length, 2);

  fake.choose((ids) => ids.find((id) => id === second.passkey.credentialId));
  assert.equal((await v.unlock()).text(), WORDS);

  await v.removePasskey(second.passkey.credentialId);
  assert.equal((await v.passkeys()).length, 1);
  assert.equal(await code(v.unlock()), "cancelled", "the removed passkey is no longer offered");
  fake.choose(null);
  assert.equal(await code(v.removePasskey(first.passkey.credentialId)), "last-passkey");
});

test("portable: the passkey alone restores the vault on a device with no record", async () => {
  fake.use(PROVIDERS.icloud);
  const v = vault({ portable: true });
  const { passkey } = await v.protect(WORDS);
  assert.equal(passkey.portable, true);
  assert.deepEqual(fake.log, ["create:icloud", "get"], "one extra prompt writes the largeBlob copy");

  store = memoryVaultStore(); // a new device
  const fresh = vault({ portable: true });
  const opened = await fresh.unlock();
  assert.equal(opened.text(), WORDS);
  assert.equal(opened.restored, true);
  assert.equal(await fresh.exists(), true, "the restored record is saved locally");
});

test("removing a portable passkey prevents it from restoring itself into the local vault", async () => {
  fake.use(PROVIDERS.icloud);
  const v = vault({ portable: true });
  const first = await v.protect(WORDS);
  fake.use(PROVIDERS.yubikey);
  const second = await v.addPasskey();
  assert.equal(second.passkey.portable, true);

  await v.removePasskey(second.passkey.credentialId);
  fake.choose((ids) => ids.find((id) => id === second.passkey.credentialId));
  assert.equal(await code(v.unlock()), "wrong-passkey");
  assert.deepEqual((await v.passkeys()).map((p) => p.credentialId), [first.passkey.credentialId]);

  // A saved backup must retain this device's revocation decision, even when
  // the removed credential still carries a valid encrypted portable copy.
  const reopened = vault({ portable: true, store: memoryVaultStore() });
  await reopened.import((await v.export())!);
  assert.equal(await code(reopened.unlock()), "wrong-passkey");
  fake.choose((ids) => ids.find((id) => id === first.passkey.credentialId));
  const opened = await reopened.unlock();
  assert.equal(opened.text(), WORDS);
  opened.wipe();
});

test("removing a passkey also rejects an autofill unlock that was already waiting", async () => {
  fake.use(PROVIDERS.icloud);
  const v = vault({ portable: true });
  const first = await v.protect(WORDS);
  fake.use(PROVIDERS.yubikey);
  const second = await v.addPasskey();
  fake.choose((ids) => ids.find((id) => id === second.passkey.credentialId));
  const pending = code(v.unlock({ mediation: "conditional" }));
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(fake.conditionalPending, 1);

  await v.removePasskey(second.passkey.credentialId);
  fake.releaseConditional();
  assert.equal(await pending, "wrong-passkey");
  assert.deepEqual((await v.passkeys()).map((p) => p.credentialId), [first.passkey.credentialId]);
});

for (const portable of [false, true]) {
  test(`removal after the final read rejects unlock and wipes plaintext (portable=${portable})`, async (t) => {
    fake.use(PROVIDERS.icloud);
    const v = vault({ portable });
    const otherTab = vault({ portable });
    const first = await v.protect(WORDS);
    fake.use(PROVIDERS.yubikey);
    const second = await v.addPasskey();
    fake.choose((ids) => ids.find((id) => id === second.passkey.credentialId));
    const decrypted: Uint8Array[] = [];
    const decrypt = crypto.subtle.decrypt.bind(crypto.subtle);
    t.mock.method(crypto.subtle, "decrypt", async (...args: Parameters<typeof decrypt>) => {
      const bytes = await decrypt(...args);
      decrypted.push(new Uint8Array(bytes));
      return bytes;
    });
    const put = store.put.bind(store);
    let removed = false;
    store.put = async (record, revision, signal) => {
      // Both reads and decrypts already finished. The other tab commits first,
      // so the lastUsedAt write must not swallow its stale-revision conflict.
      if (!removed) {
        removed = true;
        await otherTab.removePasskey(second.passkey.credentialId);
      }
      return put(record, revision, signal);
    };
    assert.equal(await code(v.unlock()), "wrong-passkey");
    assert.equal(decrypted.length, 2, "the race lands after unwrapping and decrypting");
    assert.ok(decrypted.every((bytes) => bytes.every((byte) => byte === 0)), "both DEK and plaintext are wiped");
    assert.deepEqual((await v.passkeys()).map((p) => p.credentialId), [first.passkey.credentialId]);
  });
}

test("a concurrent rename does not fail unlock or overwrite newer metadata", async () => {
  const v = vault();
  const otherTab = vault();
  const { passkey } = await v.protect(WORDS);
  const put = store.put.bind(store);
  let renamed = false;
  store.put = async (record, revision, signal) => {
    if (!renamed) {
      renamed = true;
      await otherTab.renamePasskey(passkey.credentialId, "Renamed elsewhere");
    }
    return put(record, revision, signal);
  };
  const opened = await v.unlock();
  assert.equal(opened.text(), WORDS);
  opened.wipe();
  assert.equal((await v.passkeys())[0]!.name, "Renamed elsewhere");
});

test("an unchanged read-only store can still unlock without lastUsedAt persistence", async () => {
  const v = vault();
  await v.protect(WORDS);
  store.put = async () => { throw new Error("Read-only store"); };
  const opened = await v.unlock();
  assert.equal(opened.text(), WORDS);
  opened.wipe();
});

test("a failed touch cannot release the secret when the current record cannot be checked", async () => {
  const v = vault();
  await v.protect(WORDS);
  store.put = async () => {
    store.get = async () => { throw new Error("Store unavailable"); };
    throw new PasskeyError("conflict", "Another tab wrote first");
  };
  assert.equal(await code(v.unlock()), "unknown");
});

test("replacement after decrypt rejects the old secret even when the user ID is unchanged", async () => {
  const v = vault();
  const { passkey } = await v.protect(WORDS);
  const replacementStore = memoryVaultStore();
  await vault({ store: replacementStore }).protect("a replacement secret");
  const replacement = (await replacementStore.get(v.id))!;
  fake.choose((ids) => ids.find((id) => id === passkey.credentialId));
  const put = store.put.bind(store);
  store.put = async (record, revision, signal) => {
    await put({ ...replacement, userId: record.userId, revision: record.revision + 1 }, revision);
    return put(record, revision, signal);
  };
  assert.equal(await code(v.unlock()), "conflict");
  assert.deepEqual((await store.get(v.id))!.data, replacement.data);
});

test("forget during a portable restore rejects unlock instead of returning the forgotten secret", async () => {
  fake.use(PROVIDERS.icloud);
  await vault({ portable: true }).protect(WORDS);
  store = memoryVaultStore();
  const v = vault({ portable: true });
  const put = store.put.bind(store);
  let forgotten = false;
  store.put = async (record, revision, signal) => {
    if (!forgotten) {
      forgotten = true;
      await vault({ portable: true }).forget();
    }
    return put(record, revision, signal);
  };
  assert.equal(await code(v.unlock()), "conflict");
  assert.equal(await v.exists(), false);
});

test("local removal cannot revoke an existing portable copy on a device without the record", async () => {
  fake.use(PROVIDERS.icloud);
  const v = vault({ portable: true });
  await v.protect(WORDS);
  fake.use(PROVIDERS.yubikey);
  const second = await v.addPasskey();
  await v.removePasskey(second.passkey.credentialId);
  fake.choose((ids) => ids.find((id) => id === second.passkey.credentialId));
  assert.equal(await code(v.unlock()), "wrong-passkey");
  const fresh = vault({ portable: true, store: memoryVaultStore() });
  const opened = await fresh.unlock();
  assert.equal(opened.text(), WORDS);
  assert.equal(opened.restored, true);
  opened.wipe();
});

test("no vault and not portable: fail before showing any prompt", async () => {
  const v = vault();
  assert.equal(await code(v.unlock()), "no-vault");
  assert.deepEqual(fake.log, []);
});

test("protect refuses to overwrite without replace", async () => {
  const v = vault();
  await v.protect(WORDS);
  assert.equal(await code(v.protect("other words")), "vault-exists");
  await v.protect("other words entirely", { replace: true });
  fake.choose((ids) => ids.at(-1));
  assert.equal((await v.unlock()).text(), "other words entirely");
});

test("protect replace returns the vault it overwrote so a failed setup can put it back", async () => {
  const v = vault();
  await v.protect(WORDS);
  const saved = await v.export();
  const next = await v.protect("other words entirely", { replace: true });
  assert.equal(next.replaced?.data.ct, saved?.data.ct);
  await v.import(next.replaced!, { replace: true });
  fake.choose((ids) => ids[0]);
  assert.equal((await v.unlock()).text(), WORDS);
});

test("user cancel and caller abort are distinguishable and both count as cancel", async () => {
  const v = vault();
  await v.protect(WORDS);
  fake.cancelNext("get");
  assert.equal(await code(v.unlock()), "cancelled");

  const controller = new AbortController();
  controller.abort();
  const err = await v.unlock({ signal: controller.signal }).catch((e) => e);
  assert.equal(err.code, "aborted");
  assert.ok(isPasskeyCancel(err));
});

test("an explicit unlock replaces a pending autofill request", async () => {
  const v = vault();
  await v.protect(WORDS);
  const autofill = code(v.unlock({ mediation: "conditional" }));
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(fake.conditionalPending, 1);
  const explicit = await v.unlock();
  assert.equal(explicit.text(), WORDS);
  assert.equal(await autofill, "aborted");
  const cond = fake.extensionLog.find((e) => e.op === "get" && e.mediation === "conditional");
  assert.ok(cond);
});

test("autofill unlock resolves when the user picks a passkey", async () => {
  const v = vault();
  await v.protect(WORDS);
  const autofill = v.unlock({ mediation: "conditional" });
  await new Promise((r) => setTimeout(r, 0));
  fake.releaseConditional();
  assert.equal((await autofill).text(), WORDS);
});

test("providers that reject an extension fall back instead of failing", async () => {
  fake.use(PROVIDERS.oldManager);
  const v = vault({ portable: true });
  await v.protect(WORDS);
  assert.equal((await v.unlock()).text(), WORDS);
  const creates = fake.extensionLog.filter((e) => e.op === "create");
  assert.equal(creates.length, 2, "retried create without largeBlob");
});

test("every ceremony requires user verification (PRF differs with and without UV)", async () => {
  const seen: string[] = [];
  const credentials = navigator.credentials as unknown as { create: Function; get: Function };
  const create = credentials.create.bind(credentials);
  const get = credentials.get.bind(credentials);
  credentials.create = (o: CredentialCreationOptions) => {
    seen.push(String(o.publicKey!.authenticatorSelection!.userVerification));
    return create(o);
  };
  credentials.get = (o: CredentialRequestOptions) => {
    seen.push(String(o.publicKey!.userVerification));
    return get(o);
  };
  fake.use(PROVIDERS.yubikey);
  const v = vault();
  await v.protect(WORDS);
  await v.unlock();
  assert.deepEqual(seen, ["required", "required", "required"]);
});

test("tampered ciphertext and foreign purposes fail closed", async () => {
  const v = vault();
  await v.protect(WORDS);
  const record = (await v.export())!;

  const tampered = structuredClone(record);
  const ct = Buffer.from(tampered.data.ct, "base64url");
  ct[0] = ct[0]! ^ 1;
  tampered.data.ct = ct.toString("base64url");
  store = memoryVaultStore([tampered]);
  assert.equal(await code(vault().unlock()), "decrypt-failed");

  // Same passkey, different purpose: keys are separated, so the record does not open.
  store = memoryVaultStore([{ ...record, purpose: "other" }]);
  assert.equal(await code(vault({ purpose: "other" }).unlock()), "decrypt-failed");
});

test("rename, export and import round-trip; import checks the vault identity", async () => {
  const v = vault();
  const { passkey } = await v.protect(WORDS);
  await v.renamePasskey(passkey.credentialId, "  Work laptop ");
  assert.equal((await v.passkeys())[0]!.name, "Work laptop");
  const record = (await v.export())!;

  store = memoryVaultStore();
  const other = vault();
  await other.import(record);
  assert.equal((await other.unlock()).text(), WORDS);
  assert.equal(await code(other.import(record)), "vault-exists");
  assert.equal(await code(vault({ purpose: "chat-keys" }).import(record)), "wrong-passkey");
});

test("forgotten records cannot replace a usable vault through import", async () => {
  const v = vault();
  await v.protect(WORDS);
  const live = (await v.export())!;
  await v.forget();
  const tombstone = (await store.get("default"))!;
  assert.equal(tombstone.forgotten, true);
  assert.equal(await code(store.put(live, undefined)), "conflict", "a tombstone is not an absent row");
  assert.equal(await code(v.import(tombstone)), "no-vault");
  await v.import(live);
  const before = await v.export();
  assert.equal(await code(v.import(tombstone, { replace: true })), "no-vault");
  assert.deepEqual(await v.export(), before, "a refused tombstone leaves live recovery intact");
  const unlocked = await v.unlock();
  try { assert.equal(unlocked.text(), WORDS); } finally { unlocked.wipe(); }
});

test("stores reject stale writes (two tabs)", async () => {
  const v = vault();
  await v.protect(WORDS);
  const record = (await v.export())!;
  await store.put({ ...record, revision: record.revision + 1 }, record.revision);
  assert.equal(await code(store.put({ ...record, revision: record.revision + 1 }, record.revision)), "conflict");
});

test("indexedDbVaultStore persists and enforces compare-and-swap", async () => {
  const fakeDb = memoryIndexedDb();
  Object.defineProperty(globalThis, "indexedDB", { value: fakeDb.indexedDB, configurable: true });
  try {
    store = indexedDbVaultStore({ dbName: "t" });
    const v = vault();
    await v.protect(WORDS);
    assert.equal((await v.unlock()).text(), WORDS);
    const record = (await store.get("default"))!;
    assert.equal(await code(store.put(record, record.revision - 1)), "conflict");
    await v.forget();
    assert.equal(await v.exists(), false);
  } finally {
    Reflect.deleteProperty(globalThis, "indexedDB");
  }
});

test("getPasskeySupport reads extension:* capability keys and reports why not", async () => {
  const s = await getPasskeySupport({ rpId: RP });
  assert.equal(s.available, true);
  assert.equal(s.prf, true);
  assert.equal(s.largeBlob, false);
  assert.equal(s.conditionalGet, true);
  assert.equal(s.signals.unknownCredential, true);
  Object.defineProperty(globalThis, "isSecureContext", { value: false, configurable: true });
  assert.equal((await getPasskeySupport({ rpId: RP })).reason, "insecure-context");
  fake.uninstall();
  assert.equal((await getPasskeySupport({ rpId: RP })).reason, "no-webauthn");
  fake = fakeWebAuthn();
});

test("resolveRpId: localhost yes, IP addresses no", () => {
  assert.equal(resolveRpId("localhost"), "localhost");
  assert.equal(resolveRpId("app.localhost"), "localhost");
  assert.equal(resolveRpId("Wallet.Example"), "wallet.example");
  assert.equal(resolveRpId("127.0.0.1"), null);
  assert.equal(resolveRpId("[::1]"), null);
});

test("parseAuthenticatorData reads flags and the AAGUID", () => {
  const data = new Uint8Array(55);
  data[32] = 0x01 | 0x04 | 0x08 | 0x40;
  data.set(Buffer.from("fbfc3007154e4ecc8c0b6e020557d7bd", "hex"), 37);
  const parsed = parseAuthenticatorData(data)!;
  assert.deepEqual(parsed.flags, { userVerified: true, backupEligible: true, backedUp: false });
  assert.equal(parsed.aaguid, "fbfc3007-154e-4ecc-8c0b-6e020557d7bd");
  assert.equal(parseAuthenticatorData(new Uint8Array(10)), null);
});

test("a vault needs an rpName and a usable RP ID", async () => {
  assert.throws(() => createPasskeyVault({ rpName: "" }), PasskeyError);
  const v = createPasskeyVault({ rpName: "x", store });
  assert.equal(await code(v.protect(WORDS)), "rp-id");
});

test("protect({ replace }) reaches navigator.credentials.create with no await (keeps the user gesture)", async () => {
  const credentials = navigator.credentials as unknown as { create: Function };
  const create = credentials.create.bind(credentials);
  let calledSync = false;
  let inCall = true;
  credentials.create = (o: CredentialCreationOptions) => {
    calledSync = inCall;
    return create(o);
  };
  const pending = vault().protect(WORDS, { replace: true });
  inCall = false;
  await pending;
  assert.equal(calledSync, true);
});

test("prepareAddPasskey().register() reaches credentials.create with no await", async () => {
  const v = vault();
  await v.protect(WORDS);
  const prepared = await v.prepareAddPasskey();
  fake.use(PROVIDERS.yubikey);
  const credentials = navigator.credentials as unknown as { create: Function };
  const create = credentials.create.bind(credentials);
  let calledSync = false;
  let inCall = true;
  credentials.create = (o: CredentialCreationOptions) => {
    calledSync = inCall;
    return create(o);
  };
  try {
    const pending = prepared.register();
    inCall = false;
    await pending;
    assert.equal(calledSync, true);
  } finally {
    credentials.create = create;
  }
});

test("an abort during encryption rejects at once and writes nothing", async (t) => {
  const v = vault();
  const controller = new AbortController();
  let release!: () => void;
  const gate = new Promise<void>((r) => (release = r));
  const encrypt = crypto.subtle.encrypt.bind(crypto.subtle);
  t.mock.method(crypto.subtle, "encrypt", async (...args: Parameters<typeof encrypt>) => {
    controller.abort();
    await gate;
    return encrypt(...args);
  });
  const err = await v.protect(WORDS, { signal: controller.signal }).catch((e) => e);
  assert.equal(err.code, "aborted");
  release();
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(await v.exists(), false, "no record after the caller saw an abort");
  assert.equal(fake.signalled.length, 1, "the orphaned credential is signalled");
});

test("string secrets round-trip exactly, whitespace included", async () => {
  const v = vault();
  await v.protect("  a secret\n");
  assert.equal((await v.unlock()).text(), "  a secret\n");
});

test("addPasskey will not wrap an old data key into a vault another tab replaced", async () => {
  const v = vault();
  await v.protect(WORDS);
  // What another tab's `protect({ replace: true })` leaves behind: a new secret.
  const otherStore = memoryVaultStore();
  await createPasskeyVault({ rpName: "Test Wallet", rpId: RP, store: otherStore }).protect("an entirely different secret");
  const replacement = (await otherStore.get("default"))!;
  const realGet = store.get.bind(store);
  store.get = async (id, signal) => {
    const current = await realGet(id, signal);
    // After this tab's unlock prompt, the other tab swaps the vault.
    if (current && fake.log.includes("get") && current.data !== replacement.data) {
      const swapped = { ...replacement, userId: current.userId, revision: current.revision + 1 };
      await store.put(swapped, current.revision);
      return swapped;
    }
    return current;
  };
  fake.use(PROVIDERS.yubikey);
  fake.log.length = 0;
  assert.equal(await code(v.addPasskey()), "conflict");
  assert.ok(!fake.log.some((e) => e.startsWith("create")), "no passkey is created for the wrong key");
});

test("a write cancelled while the database opens does not commit", async () => {
  const fakeDb = memoryIndexedDb();
  Object.defineProperty(globalThis, "indexedDB", { value: fakeDb.indexedDB, configurable: true });
  try {
    const idb = indexedDbVaultStore({ dbName: "t-abort" });
    const memory = memoryVaultStore();
    await createPasskeyVault({ rpName: "Test Wallet", rpId: RP, store: memory }).protect(WORDS);
    const record = (await memory.get("default"))!;
    fakeDb.holdOpens = true;
    const controller = new AbortController();
    const put = idb.put(record, undefined, controller.signal);
    controller.abort();
    fakeDb.holdOpens = false;
    while (fakeDb.opens.length) fakeDb.opens.shift()!();
    await assert.rejects(put);
    assert.equal(await idb.get("default"), undefined);
  } finally {
    Reflect.deleteProperty(globalThis, "indexedDB");
  }
});

test("addPasskey cannot overwrite a vault another tab deleted and recreated", async () => {
  const v = vault();
  await v.protect(WORDS);
  const otherStore = memoryVaultStore();
  await createPasskeyVault({ rpName: "Test Wallet", rpId: RP, store: otherStore }).protect("new secret");
  const replacement = (await otherStore.get("default"))!;
  const put = store.put.bind(store);
  // While this tab's registration prompt is open, the other tab forgets the
  // vault and makes a new one (a fresh record, not a revision of this one).
  store.put = async (record, revision, signal) => {
    if (record.passkeys.length === 2) {
      await store.delete("default");
      await put(replacement, undefined);
    }
    return put(record, revision, signal);
  };
  fake.use(PROVIDERS.yubikey);
  assert.equal(await code(v.addPasskey()), "conflict");
  assert.equal((await store.get("default"))!.userId, replacement.userId, "the new vault survives");
});

test("no portable copy under a parent-domain RP ID (sibling subdomains could read it)", async (t) => {
  Object.defineProperty(globalThis, "location", {
    value: { hostname: "wallet.example.com", host: "wallet.example.com", protocol: "https:" },
    configurable: true,
  });
  t.after(() => Reflect.deleteProperty(globalThis, "location"));
  fake.use(PROVIDERS.yubikey);
  const shared = createPasskeyVault({ rpName: "Test Wallet", rpId: "example.com", store, portable: true });
  const { passkey } = await shared.protect(WORDS);
  assert.equal(passkey.portable, false);
  assert.ok(!fake.log.some((e) => e.includes("largeBlob")), fake.log.join(","));
  const exact = createPasskeyVault({ rpName: "Test Wallet", rpId: "wallet.example.com", store: memoryVaultStore(), portable: true });
  assert.equal((await exact.protect(WORDS)).passkey.portable, true, "the page's own hostname keeps it");
});

test("a conflicting addPasskey writes no portable copy of the old vault", async () => {
  const v = vault({ portable: true });
  await v.protect(WORDS);
  const otherStore = memoryVaultStore();
  await createPasskeyVault({ rpName: "Test Wallet", rpId: RP, store: otherStore }).protect("new secret");
  const replacement = (await otherStore.get("default"))!;
  const put = store.put.bind(store);
  store.put = async (record, revision, signal) => {
    if (record.passkeys.length === 2) {
      await store.delete("default");
      await put(replacement, undefined);
    }
    return put(record, revision, signal);
  };
  const creds = navigator.credentials as CredentialsContainer;
  const realGet = creds.get.bind(creds);
  let blobWrites = 0;
  creds.get = (o?: CredentialRequestOptions) => {
    const ext = o?.publicKey?.extensions as { largeBlob?: { write?: unknown } } | undefined;
    if (ext?.largeBlob?.write) blobWrites++;
    return realGet(o);
  };
  fake.use(PROVIDERS.yubikey);
  assert.equal(await code(v.addPasskey()), "conflict");
  assert.equal(blobWrites, 0, "the old vault never reaches the new passkey");
});

function countBlobWrites(onWrite?: () => void): () => number {
  const creds = navigator.credentials as CredentialsContainer;
  const realGet = creds.get.bind(creds);
  let writes = 0;
  creds.get = (o?: CredentialRequestOptions) => {
    const ext = o?.publicKey?.extensions as { largeBlob?: { write?: unknown } } | undefined;
    if (ext?.largeBlob?.write) {
      writes++;
      onWrite?.();
    }
    return realGet(o);
  };
  return () => writes;
}

test("no portable copy on localhost (RP IDs have no port: every local server shares it)", async (t) => {
  Object.defineProperty(globalThis, "location", {
    value: { hostname: "localhost", host: "localhost:5174", protocol: "http:" },
    configurable: true,
  });
  t.after(() => Reflect.deleteProperty(globalThis, "location"));
  fake.use(PROVIDERS.yubikey);
  const local = createPasskeyVault({ rpName: "Test Wallet", rpId: "localhost", store, portable: true });
  assert.equal((await local.protect(WORDS)).passkey.portable, false);
  assert.ok(!fake.log.some((e) => e.includes("largeBlob")), fake.log.join(","));
});

test("a protect that loses to a forget writes no portable copy", async () => {
  const v = vault({ portable: true });
  const put = store.put.bind(store);
  let forgetDuringWrite = true;
  store.put = async (record, revision, signal) => {
    if (forgetDuringWrite && !record.forgotten) {
      forgetDuringWrite = false;
      await v.forget();
    }
    return put(record, revision, signal);
  };
  fake.use(PROVIDERS.yubikey);
  const blobWrites = countBlobWrites();
  assert.equal(await code(v.protect(WORDS)), "conflict");
  assert.equal(blobWrites(), 0, "the new passkey never carries a vault that was forgotten");
});

test("an abort after addPasskey saved the passkey reports it added, not cancelled", async () => {
  const v = vault({ portable: true });
  const first = await v.protect(WORDS);
  fake.use(PROVIDERS.yubikey);
  fake.choose((ids) => (ids.includes(first.passkey.credentialId) ? first.passkey.credentialId : ids[0]));
  const controller = new AbortController();
  // The caller gives up during the optional portable-copy prompt.
  countBlobWrites(() => controller.abort());
  const added = await v.addPasskey({ signal: controller.signal });
  fake.choose(null);
  assert.equal(added.passkey.portable, false);
  assert.equal((await v.passkeys()).length, 2);
});

test("forget wins over a protect that was already in flight, and a forgotten vault can be made again", async () => {
  const v = vault();
  const put = store.put.bind(store);
  let forgetDuringWrite = true;
  store.put = async (record, revision, signal) => {
    // The user forgets the vault while this tab's protect waits on its prompt.
    if (forgetDuringWrite && !record.forgotten) {
      forgetDuringWrite = false;
      await v.forget();
    }
    return put(record, revision, signal);
  };
  assert.equal(await code(v.protect(WORDS)), "conflict");
  assert.equal(await v.exists(), false, "the forget stands");
  assert.deepEqual(await v.passkeys(), []);
  assert.equal(await v.export(), undefined);
  // A later protect over the tombstone works normally.
  await v.protect(WORDS);
  assert.equal((await v.unlock()).text(), WORDS);
  await v.forget();
  assert.equal(await code(v.unlock()), "no-vault");
});

/** Device record lists passkey A; passkey B exists only as a portable copy. */
async function vaultMissingSyncedPasskey() {
  fake.use(PROVIDERS.icloud);
  const origin = vault({ portable: true });
  const first = await origin.protect(WORDS);
  fake.use(PROVIDERS.yubikey);
  fake.choose((ids) => (ids.includes(first.passkey.credentialId) ? first.passkey.credentialId : ids[0]));
  const second = await origin.addPasskey({ name: "Phone" });
  fake.choose(null);
  const full = (await origin.export())!;
  store = memoryVaultStore();
  await store.put(
    { ...full, passkeys: full.passkeys.filter((p) => p.credentialId === first.passkey.credentialId) },
    undefined,
  );
  return { first, second, device: vault({ portable: true }) };
}

test("portable unlock accepts a synced passkey that is not in the local record", async () => {
  const { second, device } = await vaultMissingSyncedPasskey();
  fake.choose((ids) => ids.find((id) => id === second.passkey.credentialId));
  const opened = await device.unlock();
  assert.equal(opened.text(), WORDS);
  assert.equal(opened.restored, true);
  assert.ok((await device.passkeys()).some((p) => p.credentialId === second.passkey.credentialId));
  const read = fake.extensionLog.find((entry) => entry.op === "get" && entry.largeBlob?.read === true);
  assert.ok(read, "the synced passkey's portable copy is read");
});

test("conditional portable unlock reads the largeBlob of a passkey outside the local record", async () => {
  const { second, device } = await vaultMissingSyncedPasskey();
  fake.choose((ids) => ids.find((id) => id === second.passkey.credentialId));
  const pending = device.unlock({ mediation: "conditional" });
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(fake.conditionalPending, 1);
  fake.releaseConditional();
  const opened = await pending;
  assert.equal(opened.text(), WORDS);
  assert.equal(opened.restored, true);
});

test("concurrent custom stores are both created", async () => {
  const idb = schemaIndexedDb();
  Object.defineProperty(globalThis, "indexedDB", { value: idb, configurable: true });
  try {
    const one = indexedDbVaultStore({ dbName: "shared", storeName: "one" });
    const two = indexedDbVaultStore({ dbName: "shared", storeName: "two" });
    await Promise.all([one.put(vaultRecord("a"), undefined), two.put(vaultRecord("b"), undefined)]);
    assert.equal((await one.get("a"))?.id, "a");
    assert.equal((await two.get("b"))?.id, "b");
  } finally {
    Reflect.deleteProperty(globalThis, "indexedDB");
  }
});

function vaultRecord(id: string): PasskeyVaultRecord {
  return {
    format: "z-stack/passkey-vault",
    version: 1,
    id,
    rpId: RP,
    purpose: "secret",
    userId: "user",
    userName: "user",
    revision: 1,
    data: { iv: "a", ct: "b" },
    passkeys: [],
    createdAt: 1,
    updatedAt: 1,
  };
}

/** IndexedDB double whose upgrades actually observe version and object stores. */
function schemaIndexedDb(): IDBFactory {
  let version = 0;
  const stores = new Map<string, Map<IDBValidKey, unknown>>();
  const database = () => ({
    version,
    close() {},
    objectStoreNames: { contains: (name: string) => stores.has(name) },
    createObjectStore(name: string) {
      if (!stores.has(name)) stores.set(name, new Map());
    },
    transaction(storeName: string, mode = "readonly") {
      const backing = stores.get(storeName);
      if (!backing) throw new DOMException(`Missing object store "${storeName}"`, "NotFoundError");
      const local = new Map(backing);
      const queue: Array<() => void> = [];
      let scheduled = false;
      let ended = false;
      const tx: {
        oncomplete: (() => void) | null;
        onabort: (() => void) | null;
        onerror: (() => void) | null;
        error: unknown;
        abort: () => void;
        objectStore: () => object;
      } = {
        oncomplete: null,
        onabort: null,
        onerror: null,
        error: null,
        abort() {
          ended = true;
          queueMicrotask(() => tx.onabort?.());
        },
        objectStore: () => ({
          get: (key: IDBValidKey) => request(() => structuredClone(local.get(key))),
          put: (value: unknown, key: IDBValidKey) => {
            const copy = structuredClone(value);
            return request(() => {
              local.set(key, copy);
              return key;
            });
          },
          delete: (key: IDBValidKey) => request(() => local.delete(key)),
        }),
      };
      const commit = () => {
        if (ended) return;
        ended = true;
        if (mode === "readwrite") {
          backing.clear();
          for (const [key, value] of local) backing.set(key, value);
        }
        tx.oncomplete?.();
      };
      const drain = () => {
        scheduled = false;
        if (ended) return;
        const step = queue.shift();
        if (step) {
          step();
          schedule();
          return;
        }
        commit();
      };
      const schedule = () => {
        if (!scheduled && !ended) {
          scheduled = true;
          queueMicrotask(drain);
        }
      };
      function request(run: () => unknown) {
        const req: { result?: unknown; error: unknown; onsuccess: (() => void) | null; onerror: (() => void) | null } = {
          error: null,
          onsuccess: null,
          onerror: null,
        };
        queue.push(() => {
          try {
            req.result = run();
            req.onsuccess?.();
          } catch (error) {
            req.error = error;
            tx.error = error;
            req.onerror?.();
            tx.onerror?.();
            tx.abort();
          }
        });
        schedule();
        return req;
      }
      schedule();
      return tx;
    },
  });
  return {
    open(_name: string, requested?: number) {
      const req: {
        result: IDBDatabase | null;
        error: DOMException | null;
        onsuccess: (() => void) | null;
        onerror: (() => void) | null;
        onupgradeneeded: (() => void) | null;
      } = { result: null, error: null, onsuccess: null, onerror: null, onupgradeneeded: null };
      queueMicrotask(() => {
        if (requested !== undefined && requested < version) {
          req.error = new DOMException("The requested version is lower than the current version.", "VersionError");
          req.onerror?.();
          return;
        }
        const upgrading = requested !== undefined && requested > version;
        if (upgrading) version = requested;
        req.result = database() as unknown as IDBDatabase;
        if (upgrading) req.onupgradeneeded?.();
        req.onsuccess?.();
      });
      return req as unknown as IDBOpenDBRequest;
    },
  } as IDBFactory;
}
