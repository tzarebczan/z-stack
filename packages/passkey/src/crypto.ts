/**
 * Envelope encryption for the vault.
 *
 *   secret ──AES-GCM(DEK)──▶ data            (once per vault)
 *   PRF(passkey, input) ──HKDF(salt)──▶ KEK ──AES-GCM──▶ wrapped DEK   (once per passkey)
 *
 * The PRF output is never used as a key directly: HKDF with a per-passkey
 * random salt and a purpose label separates it from any other use of the same
 * passkey. Each ciphertext is bound with AES-GCM additional data to the vault
 * (and passkey) it belongs to, so records cannot be swapped between vaults.
 *
 * This stays in the zero-dependency passkey package, on WebCrypto, so
 * `credentials.create` can start inside the click. Loading the Rust engine
 * first would drop that user activation.
 */

import { PasskeyError, throwIfAborted } from "./errors";
import { fromBase64Url, randomBytes, toBase64Url, utf8, wipe } from "./encoding";

export const FORMAT = "z-stack/passkey-vault";
export const FORMAT_VERSION = 1;

export type Sealed = { iv: string; ct: string };

/**
 * The PRF input: fixed per purpose so a new device can unlock in one prompt.
 * The browser hashes it ("WebAuthn PRF" || 0 || input) before the authenticator
 * sees it, so it needs no hashing here, and staying synchronous keeps
 * `create()` inside the click's user activation.
 */
export function prfInput(purpose: string): Uint8Array<ArrayBuffer> {
  return utf8(`${FORMAT}/v${FORMAT_VERSION}/prf:${purpose}`);
}

export async function deriveKek(prf: Uint8Array, salt: Uint8Array, purpose: string): Promise<CryptoKey> {
  const ikm = await crypto.subtle.importKey("raw", prf as BufferSource, "HKDF", false, ["deriveKey"]);
  return crypto.subtle.deriveKey(
    { name: "HKDF", hash: "SHA-256", salt: salt as BufferSource, info: utf8(`${FORMAT}/v${FORMAT_VERSION}/kek:${purpose}`) },
    ikm,
    { name: "AES-GCM", length: 256 },
    false,
    ["encrypt", "decrypt"],
  );
}

async function importDek(dek: Uint8Array): Promise<CryptoKey> {
  return crypto.subtle.importKey("raw", dek as BufferSource, { name: "AES-GCM" }, false, ["encrypt", "decrypt"]);
}

export async function seal(key: CryptoKey, plaintext: Uint8Array, aad: string): Promise<Sealed> {
  const iv = randomBytes(12);
  const ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: utf8(aad) }, key, plaintext as BufferSource);
  return { iv: toBase64Url(iv), ct: toBase64Url(ct) };
}

export async function open(key: CryptoKey, sealed: Sealed, aad: string): Promise<Uint8Array<ArrayBuffer>> {
  try {
    return new Uint8Array(
      await crypto.subtle.decrypt(
        { name: "AES-GCM", iv: fromBase64Url(sealed.iv), additionalData: utf8(aad) },
        key,
        fromBase64Url(sealed.ct),
      ),
    );
  } catch (error) {
    throw new PasskeyError("decrypt-failed", "This passkey could not open the vault.", { cause: error });
  }
}

export function dataAad(vaultId: string, rpId: string, purpose: string): string {
  return `${FORMAT}/v${FORMAT_VERSION}/data\n${vaultId}\n${rpId}\n${purpose}`;
}

export function keyAad(vaultId: string, rpId: string, credentialId: string): string {
  return `${FORMAT}/v${FORMAT_VERSION}/key\n${vaultId}\n${rpId}\n${credentialId}`;
}

/** Encrypt `secret` under a fresh data key. The caller wraps and then wipes `dek`. */
export async function sealSecret(
  secret: Uint8Array,
  aad: string,
  signal?: AbortSignal,
): Promise<{ dek: Uint8Array<ArrayBuffer>; data: Sealed }> {
  const dek = randomBytes(32);
  try {
    const data = await seal(await importDek(dek), secret, aad);
    throwIfAborted(signal);
    return { dek, data };
  } catch (error) {
    wipe(dek);
    throw error;
  }
}

export async function openSecret(dek: Uint8Array, data: Sealed, aad: string): Promise<Uint8Array<ArrayBuffer>> {
  return open(await importDek(dek), data, aad);
}

export async function wrapDek(
  dek: Uint8Array,
  prf: Uint8Array,
  purpose: string,
  aad: string,
): Promise<{ salt: string; wrapped: Sealed }> {
  const salt = randomBytes(32);
  const wrapped = await seal(await deriveKek(prf, salt, purpose), dek, aad);
  return { salt: toBase64Url(salt), wrapped };
}

export async function unwrapDek(
  prf: Uint8Array,
  salt: string,
  wrapped: Sealed,
  purpose: string,
  aad: string,
): Promise<Uint8Array<ArrayBuffer>> {
  return open(await deriveKek(prf, fromBase64Url(salt), purpose), wrapped, aad);
}
