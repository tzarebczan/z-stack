/**
 * Optional account registration and encrypted backup integration.
 *
 * This entry point does not initialize the wallet engine, make requests, or
 * select a backend. Applications supply their own account provider and stores.
 * Local passkey protection does not require account registration.
 *
 * @module @z-stack/sdk/services
 */
import type {
  AuthenticationResponseJSON,
  CreationOptionsJSON,
  RegistrationResponseJSON,
  RequestOptionsJSON,
} from "@z-stack/passkey";
import type { PasskeyVault, UnlockOptions } from "@z-stack/passkey";
import type { WalletUnlocker } from "./create-wallet";

/** Adapt a local passkey vault. Configure its store/RP separately; no backend is selected. */
export function passkeyUnlocker(vault: PasskeyVault, options: Omit<UnlockOptions, "signal"> = {}): WalletUnlocker {
  return {
    async unlock({ signal }) {
      const secret = await vault.unlock({ ...options, signal });
      try { return secret.text(); }
      finally { secret.wipe(); }
    },
  };
}

// Optional conveniences for the SDK's local encrypted seed vault. Applications
// may instead implement WalletUnlocker around any local keystore of their own.
export { persistEncryptedSeed, unlockEncryptedSeed, hasEncryptedSeed, forgetEncryptedSeed } from "./seed-vault";
export {
  configurePasskeys, registerPasskeySeed, tryRegisterPasskeySeed, unlockPasskeySeed,
  forgetPasskeySeed, hasPasskeySeed, passkeyInfo, passkeyCapabilities, isPasskeyAbort,
  type PasskeyCapabilities, type PasskeyConfig, type PasskeyInfo,
  type PasskeyMode, type PasskeyRegisterResult,
} from "./passkey";

/** Server options and an opaque identifier for a short-lived, single-use challenge. */
export interface AccountChallenge<Options> {
  requestId: string;
  options: Options;
}

/**
 * An application-owned account service. Implement with your own endpoints,
 * cookies or tokens; the SDK does not choose an account provider.
 *
 * The server must bind each challenge to the intended account/session, verify
 * origin, RP ID, signature and user verification, and reject replay. A locally
 * created passkey or unlocked vault is not proof of a server session.
 *
 * Challenges are single-use; fetch fresh options after an attempt or expiry.
 * Fetch options before the user clicks the ceremony button. Pass server options
 * to the vault or WebAuthn helpers from that click, then submit the sanitized
 * response for verification. Only verification establishes the remote session.
 */
export interface PasskeyAccountProvider<Session> {
  registrationOptions(signal?: AbortSignal): Promise<AccountChallenge<CreationOptionsJSON>>;
  verifyRegistration(requestId: string, response: RegistrationResponseJSON, signal?: AbortSignal): Promise<Session>;
  authenticationOptions(signal?: AbortSignal): Promise<AccountChallenge<RequestOptionsJSON>>;
  verifyAuthentication(requestId: string, response: AuthenticationResponseJSON, signal?: AbortSignal): Promise<Session>;
  /** Invalidate the server session. Closing a wallet does not call this. */
  logout(signal?: AbortSignal): Promise<void>;
}

// Existing client primitives, without wallet engine or backend imports.
export {
  createPasskeyVault,
  getPasskeySupport,
  resolveRpId,
  fromBase64Url,
  createCredential,
  getAssertion,
  registrationJson,
  assertionJson,
  indexedDbVaultStore,
  memoryVaultStore,
  PasskeyError,
  isPasskeyError,
  isPasskeyCancel,
  type PasskeyVault,
  type PasskeyVaultOptions,
  type PasskeyVaultRecord,
  type PasskeyVaultStore,
  type RegisterOptions,
  type UnlockOptions,
  type Registered,
  type Unlocked,
  type PreparedAddPasskey,
  type AuthenticationResponseJSON,
  type CreationOptionsJSON,
  type RegistrationResponseJSON,
  type RequestOptionsJSON,
} from "@z-stack/passkey";
