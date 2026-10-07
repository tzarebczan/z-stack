/** @module @z-stack/passkey */
/**
 * @z-stack/passkey — protect a secret with passkeys, correctly.
 *
 * @example
 * ```ts
 * import { createPasskeyVault, getPasskeySupport, isPasskeyCancel } from "@z-stack/passkey";
 *
 * const support = await getPasskeySupport();
 * if (support.available) {
 *   const vault = createPasskeyVault({ rpName: "Acme Wallet" });
 *   await vault.protect(mnemonic);             // on "Protect with passkey" click
 *   const { text } = await vault.unlock();     // on "Unlock" click
 * }
 * ```
 */

export {
  createPasskeyVault,
  type PasskeyInfo,
  type PasskeyVault,
  type PreparedAddPasskey,
  type PasskeyVaultOptions,
  type Registered,
  type RegisterOptions,
  type Unlocked,
  type UnlockOptions,
} from "./vault";
export { getPasskeySupport, localhostUrl, resolveRpId, type PasskeySupport } from "./support";
export { PasskeyError, isPasskeyCancel, isPasskeyError, type PasskeyErrorCode } from "./errors";
export {
  indexedDbVaultStore,
  memoryVaultStore,
  type PasskeyVaultRecord,
  type PasskeyVaultStore,
  type StoredPasskey,
} from "./store";
export { passkeyProviderName } from "./providers";
export {
  createCredential,
  getAssertion,
  assertionJson,
  parseAuthenticatorData,
  registrationJson,
  signalAllAcceptedCredentials,
  signalCurrentUserDetails,
  signalUnknownCredential,
  type AuthenticationResponseJSON,
  type CreationOptionsJSON,
  type RegistrationResponseJSON,
  type RequestOptionsJSON,
} from "./webauthn";
export { fromBase64Url, toBase64Url } from "./encoding";
