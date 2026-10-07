import { createPasskeyVault, indexedDbVaultStore, getAssertion, assertionJson, isPasskeyCancel, PasskeyError,
  type AccountChallenge, type CreationOptionsJSON, type RequestOptionsJSON } from "@z-stack/sdk/services";
import { account, remote, request, uploadAndConfirm } from "./providers";
import "./style.css";
const element = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const status = element("status"); const phrase = element<HTMLTextAreaElement>("phrase");
const vault = createPasskeyVault({ rpName: "Backup Example", id: "example-wallet", purpose: "wallet-seed",
  store: indexedDbVaultStore({ dbName: "example-remote-backup" }), portable: false });
let registration: AccountChallenge<CreationOptionsJSON> | undefined;
let authentication: AccountChallenge<RequestOptionsJSON> | undefined;
let authenticated = false, observed: number | undefined, busy = false, closed = false;
const controller = new AbortController();
const controls = () => {
  for (const button of document.querySelectorAll<HTMLButtonElement>("button")) button.disabled = busy;
  element<HTMLButtonElement>("register").disabled ||= !registration || !phrase.value.trim();
  element<HTMLButtonElement>("sign-in").disabled ||= !authentication;
  for (const id of ["upload", "retrieve", "forget-remote", "delete-account"]) element<HTMLButtonElement>(id).disabled ||= !authenticated;
};
const run = async (action: () => Promise<void>) => {
  if (busy || closed) return; busy = true; controls();
  try { await action(); }
  catch (error) { if (!closed) status.textContent = isPasskeyCancel(error) ? "Cancelled. Prepare a fresh challenge to retry." :
    error instanceof PasskeyError && error.code === "conflict" ? "Backup changed. Retrieve it before making further changes." :
    error instanceof PasskeyError && error.code === "prf-unsupported" ? "This passkey cannot encrypt a vault. Keep your phrase and choose a PRF-capable provider." :
    "Action incomplete. Your backup has not been confirmed."; }
  finally { busy = false; if (!closed) controls(); }
};
element("prepare-register").onclick = () => void run(async () => {
  authentication = undefined; registration = undefined;
  registration = await account.registrationOptions(controller.signal);
  if (!closed) status.textContent = "Ready. Create your passkey from the next button.";
});
element("register").onclick = () => void run(async () => {
  const challenge = registration!; registration = undefined;
  // Ceremony starts on this click, before any network request.
  const registered = await vault.protect(phrase.value.trim(), { server: challenge.options, signal: controller.signal,
    userName: "Example wallet", userDisplayName: "Example wallet", name: "Example wallet" });
  phrase.value = "";
  if (!closed) status.textContent = "Vault saved locally. Verifying the account…";
  await account.verifyRegistration(challenge.requestId, registered.response, controller.signal);
  if (closed) return; authenticated = true; observed = undefined;
  status.textContent = "Account verified. Upload your encrypted backup next.";
});
element("prepare-sign-in").onclick = () => void run(async () => {
  registration = undefined; authentication = undefined;
  authentication = await account.authenticationOptions(controller.signal);
  if (!closed) status.textContent = "Ready. Sign in from the next button.";
});
element("sign-in").onclick = () => void run(async () => {
  const challenge = authentication!; authentication = undefined;
  const assertion = await getAssertion({ rpId: "localhost", extensions: [{}], server: challenge.options, signal: controller.signal });
  await account.verifyAuthentication(challenge.requestId, assertionJson(assertion.credential), controller.signal);
  const saved = await remote.get("example-wallet", controller.signal);
  if (closed) return; authenticated = true; observed = saved?.revision;
  status.textContent = saved?.forgotten ? "The remote backup was forgotten." : saved ? "Signed in. Retrieve the backup to restore this browser." : "Signed in. No remote backup exists yet.";
});
element("upload").onclick = () => void run(async () => {
  const record = await vault.export(controller.signal);
  if (!record) throw new Error("no-local-vault");
  status.textContent = "Uploading encrypted backup…";
  observed = await uploadAndConfirm(record, observed, controller.signal);
  if (!closed) status.textContent = "Backup confirmed: uploaded and retrieved successfully.";
});
element("retrieve").onclick = () => void run(async () => {
  const record = await remote.get("example-wallet", controller.signal);
  if (!record || record.forgotten) {
    if (closed) return;
    observed = record?.revision;
    status.textContent = record?.forgotten ? "Remote backup forgotten. Local copies remain." : "No remote backup exists.";
    return;
  }
  await vault.import(record, { signal: controller.signal }); // Existing local vault requires deliberate reconciliation.
  if (closed) return; observed = record.revision;
  status.textContent = "Encrypted backup restored. Unlock it from the next button.";
});
element("unlock").onclick = () => void run(async () => {
  const unlocked = await vault.unlock({ signal: controller.signal });
  try { if (!closed) status.textContent = "Recovered locally. Decrypted bytes were not sent to the server."; }
  finally { unlocked.wipe(); }
});
element("forget-local").onclick = () => void run(async () => {
  await vault.forget(controller.signal); phrase.value = "";
  if (!closed) status.textContent = "Local vault forgotten. The remote backup is unchanged.";
});
element("forget-remote").onclick = () => void run(async () => {
  const record = await remote.get("example-wallet", controller.signal);
  if (record?.forgotten) {
    if (closed) return;
    observed = record.revision;
    status.textContent = "Remote backup forgotten. Older writes cannot recreate it.";
    return;
  }
  if (!record || record.revision !== observed) throw new PasskeyError("conflict", "Backup changed.");
  const tombstone = { ...record, forgotten: true as const, revision: record.revision + 1, updatedAt: Date.now(),
    userId: "", userName: "", data: { iv: "", ct: "" }, passkeys: [] };
  observed = await uploadAndConfirm(tombstone, observed, controller.signal);
  if (!closed) status.textContent = "Remote backup forgotten. Older writes cannot recreate it.";
});
element("logout").onclick = () => void run(async () => {
  await account.logout(controller.signal); authenticated = false; observed = undefined; registration = undefined; authentication = undefined;
  phrase.value = ""; if (!closed) status.textContent = "Signed out. Local vault and remote backup remain.";
});
element("delete-account").onclick = () => void run(async () => {
  const result = await request("/api/account", "DELETE", {}, controller.signal); if (!result.ok) throw new Error("delete-failed");
  authenticated = false; observed = undefined; phrase.value = "";
  if (!closed) status.textContent = "Account, credentials, sessions and remote backup deleted. Local copies remain.";
});
phrase.oninput = controls;
window.addEventListener("pagehide", () => { closed = true; controller.abort(); phrase.value = ""; }, { once: true });
controls();
