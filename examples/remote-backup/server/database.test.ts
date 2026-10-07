import assert from "node:assert/strict";
import { test } from "node:test";
import { BackupDatabase } from "./database.ts";
import { createBackupServer } from "./app.ts";
import { validateRecord } from "./record.ts";
import type { PasskeyVaultRecord } from "@z-stack/sdk/services";
const record = (revision: number): PasskeyVaultRecord => ({ format: "z-stack/passkey-vault", version: 1,
  id: "example-wallet", rpId: "localhost", purpose: "wallet-seed", userId: "YQ", userName: "Example wallet", revision,
  data: { iv: "AAAAAAAAAAAAAAAA", ct: "AAAAAAAAAAAAAAAAAAAAAA" }, passkeys: [{ credentialId: "YQ", salt: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    wrapped: { iv: "AAAAAAAAAAAAAAAA", ct: "AAAAAAAAAAAAAAAAAAAAAA" }, createdAt: 1 }], createdAt: 1, updatedAt: revision });
test("account-scoped backups, revision checks, forget tombstones and cascading deletion", () => {
  const database = new BackupDatabase(":memory:");
  try {
    for (const owner of ["a", "b"]) database.register(owner, { id: owner, publicKey: new Uint8Array([1]), counter: 0 });
    database.session("a-session", "a"); database.session("b-session", "b");
    database.put("a", record(1), undefined);
    assert.equal(database.get("b", "example-wallet"), undefined);
    assert.throws(() => database.put("a", record(2), undefined), /revision-conflict/);
    database.put("a", record(2), 1);
    assert.throws(() => database.put("a", record(3), 1), /revision-conflict/);
    const forgotten: PasskeyVaultRecord = { ...record(3), forgotten: true, userId: "", userName: "", passkeys: [], data: { iv: "", ct: "" } };
    database.put("a", validateRecord(forgotten), 2);
    assert.throws(() => database.put("a", record(3), 2), /revision-conflict/);
    assert.equal(database.get("a", "example-wallet")?.forgotten, true);
    database.deleteAccount("a");
    assert.equal(database.owner("a-session"), undefined); assert.equal(database.credential("a"), undefined);
    assert.equal(database.get("a", "example-wallet"), undefined); assert.equal(database.owner("b-session"), "b");
    assert.throws(() => database.put("a", record(1), undefined), /FOREIGN KEY/);
  } finally { database.close(); }
});
test("record validation rejects unknown plaintext fields, malformed crypto metadata and incomplete tombstones", () => {
  assert.deepEqual(validateRecord(record(1)), record(1));
  for (const invalid of [{ ...record(1), mnemonic: "never-store-this" }, { ...record(1), data: { iv: "", ct: "" } },
    { ...record(1), revision: 1.5 }, { ...record(1), passkeys: [] }, { ...record(1), forgotten: true }]) {
    assert.throws(() => validateRecord(invalid), /invalid-record/);
  }
});
test("HTTP endpoints reject missing sessions, foreign origins and replayed failed ceremonies", async () => {
  const database = new BackupDatabase(":memory:"); const server = createBackupServer(database);
  await new Promise<void>(resolve => server.listen(0, "127.0.0.1", resolve));
  const base = `http://127.0.0.1:${(server.address() as { port: number }).port}`;
  const headers = { "X-Example-Request": "1", Origin: "http://localhost:5173", "Content-Type": "application/json" };
  try {
    assert.equal((await fetch(`${base}/api/backups/example-wallet`, { headers })).status, 401);
    assert.equal((await fetch(`${base}/api/register/options`, { method: "POST", headers: { ...headers, Origin: "https://foreign.example" }, body: "{}" })).status, 403);
    const options = await fetch(`${base}/api/register/options`, { method: "POST", headers, body: "{}" });
    const { requestId } = await options.json() as { requestId: string };
    const cookie = options.headers.get("set-cookie")!.split(";")[0];
    const authOptions = await fetch(`${base}/api/authenticate/options`, { method: "POST", headers, body: "{}" });
    const authCookie = authOptions.headers.get("set-cookie")!.split(";")[0];
    assert.notEqual(cookie.split("=")[0], authCookie.split("=")[0], "ceremonies must have independent cookie bindings");
    const verify = () => fetch(`${base}/api/register/verify`, { method: "POST", headers: { ...headers, Cookie: `${cookie}; ${authCookie}` }, body: JSON.stringify({ requestId, response: {} }) });
    const failed = await verify(); assert.equal(failed.status, 401);
    assert.equal((await failed.json() as { code: string }).code, "verification-failed", "the original challenge survived preparing another ceremony");
    const replay = await verify(); assert.equal(replay.status, 401);
    assert.equal((await replay.json() as { code: string }).code, "invalid-challenge");
  } finally { await new Promise<void>(resolve => server.close(() => resolve())); database.close(); }
});
