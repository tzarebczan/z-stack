import { PasskeyError, type PasskeyAccountProvider, type PasskeyVaultStore, type PasskeyVaultRecord } from "@z-stack/sdk/services";
import { validateRecord } from "../server/record.ts";
export async function request(path: string, method = "GET", body?: unknown, signal?: AbortSignal): Promise<Response> {
  return fetch(path, { method, credentials: "same-origin", cache: "no-store", redirect: "error", referrerPolicy: "no-referrer", signal,
    headers: { "X-Example-Request": "1", ...(body === undefined ? {} : { "Content-Type": "application/json" }) },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }) });
}
async function json(response: Response): Promise<unknown> {
  // The server bounds responses too; this independently bounds downloads.
  if (!response.body) throw new Error("invalid-response");
  const reader = response.body.getReader(); const chunks: Uint8Array[] = []; let length = 0;
  try {
    for (;;) { const { value, done } = await reader.read(); if (done) break;
      length += value.length; if (length > 256 * 1024) throw new Error("invalid-response"); chunks.push(value); }
  } finally { await reader.cancel().catch(() => {}); reader.releaseLock(); }
  const data = new Uint8Array(length); let offset = 0;
  for (const chunk of chunks) { data.set(chunk, offset); offset += chunk.length; }
  return JSON.parse(new TextDecoder().decode(data));
}
export const account: PasskeyAccountProvider<void> = {
  async registrationOptions(signal) {
    const response = await request("/api/register/options", "POST", {}, signal);
    if (!response.ok) throw new Error("account-unavailable");
    return await json(response) as Awaited<ReturnType<typeof account.registrationOptions>>;
  },
  async verifyRegistration(requestId, response, signal) {
    const result = await request("/api/register/verify", "POST", { requestId, response }, signal);
    if (!result.ok || (await json(result) as { authenticated?: unknown } | null)?.authenticated !== true) throw new Error("verification-failed");
  },
  async authenticationOptions(signal) {
    const response = await request("/api/authenticate/options", "POST", {}, signal);
    if (!response.ok) throw new Error("account-unavailable");
    return await json(response) as Awaited<ReturnType<typeof account.authenticationOptions>>;
  },
  async verifyAuthentication(requestId, response, signal) {
    const result = await request("/api/authenticate/verify", "POST", { requestId, response }, signal);
    if (!result.ok || (await json(result) as { authenticated?: unknown } | null)?.authenticated !== true) throw new Error("verification-failed");
  },
  async logout(signal) { if (!(await request("/api/logout", "POST", {}, signal)).ok) throw new Error("logout-failed"); },
};
export const remote: PasskeyVaultStore = {
  async get(id, signal) {
    const response = await request(`/api/backups/${encodeURIComponent(id)}`, "GET", undefined, signal);
    if (response.status === 404) return undefined;
    if (!response.ok) throw new Error("backup-unavailable");
    return validateRecord(await json(response));
  },
  async put(record, expectedRevision, signal) {
    const response = await request(`/api/backups/${encodeURIComponent(record.id)}`, "PUT", { record, expectedRevision }, signal);
    if (response.status === 409) throw new PasskeyError("conflict", "Backup changed. Retrieve and reconcile it before replacing it.");
    if (!response.ok) throw new Error("backup-unconfirmed");
    const receipt = await json(response) as { revision?: number };
    if (receipt?.revision !== record.revision) throw new Error("backup-unconfirmed");
  },
  async delete() { throw new Error("Use the explicit account-deletion action; normal forget writes a tombstone."); },
};
/** Read back the exact ciphertext generation; an uncertain write never claims completion. */
export async function uploadAndConfirm(record: PasskeyVaultRecord, expectedRevision: number | undefined, signal?: AbortSignal) {
  try { await remote.put(record, expectedRevision, signal); }
  catch (error) {
    // Retry after an uncertain delivery may find that this exact request committed.
    if (!(error instanceof PasskeyError) || error.code !== "conflict") throw error;
  }
  const retrieved = await remote.get(record.id, signal);
  if (JSON.stringify(retrieved) !== JSON.stringify(record)) throw new PasskeyError("conflict", "Backup changed; the previous generation remains active.");
  return record.revision;
}
