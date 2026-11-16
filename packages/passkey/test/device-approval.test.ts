import assert from "node:assert/strict";
import test from "node:test";
import * as approval from "../src/device-approval.ts";
import { runDeviceApprovalChecks } from "../../../scripts/fixtures/device-approval-checks.mjs";

test("device approval contract uses real WebCrypto: modes, context, canonical wire, expiry, tamper, retry and concurrency", async () => {
  const checks = await runDeviceApprovalChecks(approval);
  assert.ok(checks.length >= 50);
});

test("private ECDH keys are nonextractable and owned shared-secret/plaintext buffers are wiped", async () => {
  const subtle = crypto.subtle;
  const generated = subtle.generateKey.bind(subtle), derived = subtle.deriveBits.bind(subtle), encrypted = subtle.encrypt.bind(subtle), decrypted = subtle.decrypt.bind(subtle);
  const keys: CryptoKey[] = [], shared: ArrayBuffer[] = [], plaintext: Uint8Array[] = [];
  Object.defineProperty(subtle, "generateKey", { configurable: true, value: async (...args: Parameters<typeof generated>) => {
    const result = await generated(...args);
    if ("privateKey" in result) keys.push(result.privateKey);
    return result;
  } });
  Object.defineProperty(subtle, "deriveBits", { configurable: true, value: async (...args: Parameters<typeof derived>) => { const result = await derived(...args); shared.push(result); return result; } });
  Object.defineProperty(subtle, "encrypt", { configurable: true, value: async (...args: Parameters<typeof encrypted>) => {
    const data = args[2]; plaintext.push(new Uint8Array(data instanceof ArrayBuffer ? data : data.buffer, data instanceof ArrayBuffer ? 0 : data.byteOffset, data instanceof ArrayBuffer ? data.byteLength : data.byteLength));
    return encrypted(...args);
  } });
  Object.defineProperty(subtle, "decrypt", { configurable: true, value: async (...args: Parameters<typeof decrypted>) => { const result = await decrypted(...args); plaintext.push(new Uint8Array(result)); return result; } });
  try {
    const origin = "https://wallet.example";
    const receiver = await approval.createDeviceApprovalRequest({ origin, mode: "wallet" });
    const response = await approval.sealDeviceApproval({ origin, request: receiver.request, payload: { kind: "wallet", mnemonic: Array(12).fill("fixture").join(" "), fingerprint: "aa".repeat(32), network: "mainnet", birthdayHeight: 1 } });
    await receiver.open(response);
    receiver.dispose();
    assert.equal(keys.length, 2);
    for (const key of keys) { assert.equal(key.extractable, false); await assert.rejects(subtle.exportKey("jwk", key)); }
    assert.equal(shared.length, 2);
    assert.ok(shared.every(value => new Uint8Array(value).every(byte => byte === 0)));
    assert.equal(plaintext.length, 2);
    assert.ok(plaintext.every(value => value.every(byte => byte === 0)));
  } finally {
    for (const name of ["generateKey", "deriveBits", "encrypt", "decrypt"]) Reflect.deleteProperty(subtle, name);
  }
});

test("disposal and expiry after real decryption revoke plaintext and wipe its buffer", async () => {
  const subtle = crypto.subtle, decrypt = subtle.decrypt.bind(subtle);
  const origin = "https://wallet.example";
  const payload: approval.DeviceApprovalPayload = { kind: "wallet", mnemonic: Array(12).fill("fixture").join(" "), fingerprint: "aa".repeat(32), network: "mainnet", birthdayHeight: 1 };
  let time = Date.now();
  for (const revoke of ["dispose", "expire"] as const) {
    const receiver = await approval.createDeviceApprovalRequest({ origin, mode: "wallet", ttlMs: 1000, now: () => time });
    const response = await approval.sealDeviceApproval({ origin, request: receiver.request, payload, now: () => time });
    let decrypted: Uint8Array | undefined;
    Object.defineProperty(subtle, "decrypt", { configurable: true, value: async (...args: Parameters<typeof decrypt>) => {
      const result = await decrypt(...args);
      decrypted = new Uint8Array(result);
      if (revoke === "dispose") receiver.dispose();
      else time += 1000;
      return result;
    } });
    try {
      await assert.rejects(receiver.open(response), (error: unknown) => error instanceof approval.DeviceApprovalError && error.code === (revoke === "dispose" ? "disposed" : "expired"));
      assert.ok(decrypted && decrypted.length > 0 && decrypted.every(byte => byte === 0));
    } finally { Reflect.deleteProperty(subtle, "decrypt"); receiver.dispose(); }
  }
});
