import { DatabaseSync } from "node:sqlite";
import { createHash } from "node:crypto";
import type { WebAuthnCredential } from "@simplewebauthn/server";
import type { PasskeyVaultRecord } from "@z-stack/sdk/services";

const hash = (value: string) => createHash("sha256").update(value).digest("hex");
export class BackupDatabase {
  readonly db: DatabaseSync;
  constructor(path: string) {
    this.db = new DatabaseSync(path);
    this.db.exec(`PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA busy_timeout=1000;
      CREATE TABLE IF NOT EXISTS accounts (id TEXT PRIMARY KEY);
      CREATE TABLE IF NOT EXISTS credentials (id TEXT PRIMARY KEY, owner TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, public_key BLOB NOT NULL, counter INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS sessions (hash TEXT PRIMARY KEY, owner TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, expires INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS backups (owner TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE, id TEXT NOT NULL, revision INTEGER NOT NULL, record TEXT NOT NULL, PRIMARY KEY(owner,id));`);
  }
  register(owner: string, credential: WebAuthnCredential) {
    this.db.exec("BEGIN IMMEDIATE");
    try {
      this.db.prepare("INSERT INTO accounts VALUES (?)").run(owner);
      this.db.prepare("INSERT INTO credentials VALUES (?,?,?,?)").run(credential.id, owner, credential.publicKey, credential.counter);
      this.db.exec("COMMIT");
    } catch (error) { this.db.exec("ROLLBACK"); throw error; }
  }
  credential(id: string) {
    const row = this.db.prepare("SELECT * FROM credentials WHERE id=?").get(id);
    return row ? { owner: String(row.owner), credential: { id, publicKey: new Uint8Array(row.public_key as Uint8Array), counter: Number(row.counter) } } : undefined;
  }
  advanceCounter(id: string, previous: number, next: number) {
    const result = this.db.prepare("UPDATE credentials SET counter=? WHERE id=? AND counter=?").run(next, id, previous);
    if (result.changes !== 1) throw new Error("credential-conflict");
  }
  session(token: string, owner: string) {
    this.db.prepare("DELETE FROM sessions WHERE expires<=?").run(Date.now());
    this.db.prepare("INSERT INTO sessions VALUES (?,?,?)").run(hash(token), owner, Date.now() + 30 * 60_000);
  }
  owner(token: string) {
    const row = this.db.prepare("SELECT owner FROM sessions WHERE hash=? AND expires>?").get(hash(token), Date.now());
    return row ? String(row.owner) : undefined;
  }
  logout(token: string) { this.db.prepare("DELETE FROM sessions WHERE hash=?").run(hash(token)); }
  get(owner: string, id: string): PasskeyVaultRecord | undefined {
    const row = this.db.prepare("SELECT record FROM backups WHERE owner=? AND id=?").get(owner, id);
    return row ? JSON.parse(String(row.record)) : undefined;
  }
  put(owner: string, record: PasskeyVaultRecord, expected: number | undefined) {
    this.db.exec("BEGIN IMMEDIATE");
    try {
      const current = this.get(owner, record.id);
      if (current?.revision !== expected || record.revision <= (current?.revision ?? 0)) throw new Error("revision-conflict");
      this.db.prepare("INSERT INTO backups VALUES (?,?,?,?) ON CONFLICT(owner,id) DO UPDATE SET revision=excluded.revision,record=excluded.record")
        .run(owner, record.id, record.revision, JSON.stringify(record));
      this.db.exec("COMMIT");
    } catch (error) { this.db.exec("ROLLBACK"); throw error; }
  }
  deleteAccount(owner: string) { this.db.prepare("DELETE FROM accounts WHERE id=?").run(owner); }
  close() { this.db.close(); }
}
