import type { PasskeyVaultRecord } from "@z-stack/sdk/services";
// Accept exactly the fields the server will store. Unknown fields are rejected,
// rather than allowing a client to hide plaintext or tokens alongside ciphertext.
const object = (value: unknown): Record<string, unknown> => {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("invalid-record");
  return value as Record<string, unknown>;
};
const keys = (value: Record<string, unknown>, allowed: string[]) => {
  if (Object.keys(value).some(key => !allowed.includes(key))) throw new Error("invalid-record");
};
const text = (value: unknown, max = 256, empty = false): value is string =>
  typeof value === "string" && value.length <= max && (empty || value.length > 0);
const integer = (value: unknown): value is number => Number.isSafeInteger(value) && Number(value) >= 0;
const base64 = (value: unknown, min = 1, max = 4096): value is string =>
  text(value, max, min === 0) && value.length >= min && (value === "" || /^[A-Za-z0-9_-]+$/.test(value));
const sealed = (value: unknown, forgotten: boolean) => {
  const data = object(value); keys(data, ["iv", "ct"]);
  if (forgotten ? data.iv !== "" || data.ct !== "" : !base64(data.iv, 16, 16) || !base64(data.ct, 22, 180_000)) throw new Error("invalid-record");
};
export function validateRecord(value: unknown): PasskeyVaultRecord {
  const row = object(value);
  keys(row, ["format", "version", "id", "rpId", "purpose", "userId", "userName", "revision", "data", "passkeys", "revokedCredentialIds", "createdAt", "updatedAt", "forgotten"]);
  const forgotten = row.forgotten === true;
  if (row.format !== "z-stack/passkey-vault" || row.version !== 1 || !text(row.id) || !text(row.purpose) ||
      !text(row.rpId, 253, forgotten) || !base64(row.userId, forgotten ? 0 : 1, 128) || !text(row.userName, 256, forgotten) ||
      !integer(row.revision) || row.revision < 1 || !integer(row.createdAt) || !integer(row.updatedAt) ||
      row.updatedAt < row.createdAt || (row.forgotten !== undefined && !forgotten) ||
      !Array.isArray(row.passkeys) || row.passkeys.length > 16 || (!forgotten && row.passkeys.length < 1) ||
      (forgotten && (row.passkeys.length !== 0 || row.userId !== "" || row.userName !== ""))) throw new Error("invalid-record");
  sealed(row.data, forgotten);
  const credentials = new Set<string>();
  for (const value of row.passkeys) {
    const entry = object(value);
    keys(entry, ["credentialId", "transports", "salt", "wrapped", "name", "aaguid", "backupEligible", "backedUp", "portable", "createdAt", "lastUsedAt"]);
    if (!base64(entry.credentialId) || credentials.has(entry.credentialId) || !base64(entry.salt, 43, 43) ||
      !integer(entry.createdAt) || (entry.lastUsedAt !== undefined && !integer(entry.lastUsedAt)) ||
      (entry.name !== undefined && !text(entry.name, 256, true)) ||
      (entry.aaguid !== undefined && (typeof entry.aaguid !== "string" || !/^[a-f0-9-]{36}$/i.test(entry.aaguid))) ||
      ["backupEligible", "backedUp", "portable"].some(key => entry[key] !== undefined && typeof entry[key] !== "boolean") ||
      (entry.transports !== undefined && (!Array.isArray(entry.transports) || entry.transports.length > 8 ||
        entry.transports.some(value => !["ble", "cable", "hybrid", "internal", "nfc", "smart-card", "usb"].includes(String(value)))))) throw new Error("invalid-record");
    credentials.add(entry.credentialId); sealed(entry.wrapped, false);
  }
  if (row.revokedCredentialIds !== undefined && (!Array.isArray(row.revokedCredentialIds) || row.revokedCredentialIds.length > 128 ||
      row.revokedCredentialIds.some(value => !base64(value) || credentials.has(value)))) throw new Error("invalid-record");
  return structuredClone(value) as PasskeyVaultRecord;
}
