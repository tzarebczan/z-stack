/**
 * Passkey-protected spending seed for the in-tab wallet, built on
 * `@z-stack/passkey` (PRF + envelope encryption; see its README for the
 * pitfalls it handles).
 *
 * The vault record lives in the wallet database next to the snapshot and is
 * fenced by wallet generation, like `seed.enc`: forget, a replacement wallet
 * or another tab cancels a pending ceremony, and no late write lands.
 * Old `passkey.v1` records are not supported; restore from the original phrase.
 */

import {
  createPasskeyVault,
  getPasskeySupport,
  isPasskeyCancel,
  isPasskeyError,
  localhostUrl,
  PasskeyError,
  resolveRpId,
  signalUnknownCredential,
  type PasskeyInfo as VaultPasskey,
  type PasskeyVaultRecord,
  type PasskeyVaultStore,
} from "@z-stack/passkey";
import { WalletError } from "@z-stack/core";
import { beginVaultOperation, invalidateVaultOperations } from "./vault-operation";
import { walletStorageAvailable, aborted, checkVaultGeneration, domainGenerationKey, walletTransaction, type WalletGeneration } from "./wallet-storage";

/** IndexedDB key of the vault record in the wallet store. */
export const PASSKEY_VAULT_KEY = "passkey.vault.v1";
const LEGACY_KEY = "passkey.v1";
const PURPOSE = "zcash-seed";

/** The passkey has no seed for this device, and carried no portable copy. */
export const PASSKEY_NO_SEED_BLOB =
  "this passkey has no seed blob — restore from words or a device that still has the PRF copy.";

export type PasskeyMode = "prf";

export type PasskeyCapabilities = {
  webauthn: boolean;
  secureContext: boolean;
  /** Valid WebAuthn RP ID, or null if this origin cannot host passkeys. */
  rpId: string | null;
  /** Same path on localhost when the tab is on 127.0.0.1. */
  localhostUrl?: string;
  conditional: boolean;
  largeBlob: boolean | null;
  prf: boolean | null;
};

export type PasskeyInfo = {
  credId: string;
  mode: PasskeyMode;
  transports?: string[];
  createdAt: number;
  /** Provider name ("iCloud Keychain", "1Password", …) when known. */
  provider?: string;
  /** Syncs to the user's other devices (true), device-bound (false), unknown (null). */
  synced?: boolean | null;
  /** The passkey carries an encrypted copy of the seed vault (largeBlob). */
  portable?: boolean;
};

export type PasskeyConfig = {
  /** Shown in the passkey sheet and password manager. Default: `document.title`, else "Zcash wallet". */
  rpName?: string;
  /** Account name saved with the passkey. Default: `rpName`. */
  userName?: string;
  /** WebAuthn RP ID. Default: this page's hostname. */
  rpId?: string;
};

let config: PasskeyConfig = {};

/** Name the passkeys this wallet creates (call once at startup). */
export function configurePasskeys(next: PasskeyConfig): void {
  config = { ...next };
}

function rpName(): string {
  const title = typeof document !== "undefined" ? document.title?.trim() : "";
  return config.rpName || title || "Zcash wallet";
}

type Operation = ReturnType<typeof beginVaultOperation>;

/** Vault store over the wallet database, checked against the operation's wallet generation. */
function fencedStore(op: Operation): PasskeyVaultStore {
  const conflict = () => new PasskeyError("conflict", "The passkey vault changed in another tab. Reload and try again.");
  return {
    async get(_id, signal) {
      if (!walletStorageAvailable()) return undefined;
      const generation = await op.generation;
      return walletTransaction<PasskeyVaultRecord | undefined>("readonly", tx => {
        checkVaultGeneration(tx, "passkey", generation, () => {
          tx.request(tx.store.get(PASSKEY_VAULT_KEY), value => tx.result(value as PasskeyVaultRecord | undefined));
        });
      }, signal ?? op.signal);
    },
    async put(record, expectedRevision, signal) {
      if (!walletStorageAvailable()) throw new PasskeyError("unsupported", "IndexedDB is required for the passkey vault.");
      const generation = await op.generation;
      op.assertCurrent();
      await walletTransaction<void>("readwrite", tx => {
        checkVaultGeneration(tx, "passkey", generation, () => {
          tx.request(tx.store.get(PASSKEY_VAULT_KEY), current => {
            if ((current as PasskeyVaultRecord | undefined)?.revision !== expectedRevision) throw conflict();
            tx.store.put(record, PASSKEY_VAULT_KEY);
          });
        });
      }, signal ?? op.signal);
    },
    async delete(_id, signal) {
      if (!walletStorageAvailable()) throw new PasskeyError("unsupported", "Local passkey storage is unavailable; deletion was not completed.");
      const generation = await op.generation;
      op.assertCurrent();
      await walletTransaction<void>("readwrite", tx => {
        checkVaultGeneration(tx, "passkey", generation, () => tx.store.delete(PASSKEY_VAULT_KEY));
      }, signal ?? op.signal);
    },
  };
}

function seedVault(store: PasskeyVaultStore) {
  return createPasskeyVault({
    rpName: rpName(),
    ...(config.rpId ? { rpId: config.rpId } : {}),
    purpose: PURPOSE,
    store,
    // Security keys and iCloud Keychain can carry the (encrypted) vault, so the
    // passkey alone restores the seed on a new device.
    portable: true,
  });
}

function toInfo(p: VaultPasskey): PasskeyInfo {
  return {
    credId: p.credentialId,
    mode: "prf",
    ...(p.transports ? { transports: p.transports } : {}),
    createdAt: p.createdAt,
    ...(p.provider ? { provider: p.provider } : {}),
    synced: p.synced,
    portable: p.portable,
  };
}

/**
 * Keep the SDK's cancellation contract: aborts surface as the lifecycle's
 * `AbortError` (for example "saved wallet changed"), everything else as the
 * package's typed `PasskeyError`.
 */
function sdkError(error: unknown, op: Operation): unknown {
  if (isPasskeyError(error, "aborted")) {
    const cause = error.cause ?? op.signal.reason;
    return cause instanceof Error && cause.name === "AbortError" ? cause : aborted();
  }
  if (isPasskeyError(error, "no-vault")) return new PasskeyError("no-vault", PASSKEY_NO_SEED_BLOB, { cause: error });
  return error;
}

async function readRaw(key: string, signal?: AbortSignal): Promise<unknown> {
  if (!walletStorageAvailable()) return undefined;
  return walletTransaction("readonly", tx => {
    tx.request(tx.store.get(key), value => tx.result(value));
  }, signal);
}

/**
 * Register a discoverable passkey and encrypt the seed for it. Must run from a
 * user gesture: `navigator.credentials.create` is reached before any await.
 */
export type PasskeyReplacement = {
  info: PasskeyInfo;
  credentialId: string;
  /** Vault that this registration overwrote, when one was already saved. */
  replaced?: PasskeyVaultRecord;
};

async function registerPasskeySeedRecord(
  mnemonic: string,
  signal?: AbortSignal,
  expectedGeneration?: WalletGeneration | Promise<WalletGeneration>,
): Promise<PasskeyReplacement> {
  const words = mnemonic.trim();
  if (!words) throw new Error("empty seed");
  const op = beginVaultOperation("passkey", signal, true, expectedGeneration);
  try {
    const { passkey, replaced } = await seedVault(fencedStore(op)).protect(words, {
      replace: true,
      userName: config.userName || rpName(),
      signal: op.signal,
    });
    return {
      info: toInfo(passkey),
      credentialId: passkey.credentialId,
      ...(replaced ? { replaced } : {}),
    };
  } catch (error) {
    throw sdkError(error, op);
  } finally {
    op.dispose();
  }
}

export async function registerPasskeySeed(
  mnemonic: string,
  signal?: AbortSignal,
  expectedGeneration?: WalletGeneration | Promise<WalletGeneration>,
): Promise<PasskeyInfo> {
  return (await registerPasskeySeedRecord(mnemonic, signal, expectedGeneration)).info;
}

/**
 * Put back the vault a registration overwrote, or delete the new record when
 * there was none. The new passkey is retired so it does not appear as a way
 * to open the restored vault.
 */
export async function undoPasskeySeedRegistration(
  written: { credentialId: string; replaced?: PasskeyVaultRecord },
  signal?: AbortSignal,
  expectedGeneration?: WalletGeneration | Promise<WalletGeneration>,
): Promise<void> {
  const op = beginVaultOperation("passkey", signal, false, expectedGeneration);
  try {
    const vault = seedVault(fencedStore(op));
    if (written.replaced) await vault.import(written.replaced, { replace: true, signal: op.signal });
    else await vault.forget(op.signal);
    await signalUnknownCredential(vault.rpId, written.credentialId).catch(() => undefined);
  } finally {
    op.dispose();
  }
}

export type PasskeyRegisterResult =
  | { status: "ok"; info: PasskeyInfo }
  | { status: "aborted" }
  | { status: "error"; message: string };

/** Register without throwing. Wallet create/restore should still proceed on abort or error. */
export async function tryRegisterPasskeySeed(
  mnemonic: string,
  signal?: AbortSignal,
  expectedGeneration?: WalletGeneration | Promise<WalletGeneration>,
): Promise<PasskeyRegisterResult> {
  try {
    const recorded = await registerPasskeySeedRecord(mnemonic, signal, expectedGeneration);
    return { status: "ok", info: recorded.info };
  } catch (e) {
    if (isPasskeyAbort(e)) return { status: "aborted" };
    return { status: "error", message: e instanceof Error ? e.message : String(e) };
  }
}

export type PasskeyReplacementResult =
  | ({ status: "ok" } & PasskeyReplacement)
  | { status: "aborted" }
  | { status: "error"; message: string };

/** Like {@link tryRegisterPasskeySeed}, plus enough to undo the write. */
export async function tryReplacePasskeySeed(
  mnemonic: string,
  signal?: AbortSignal,
  expectedGeneration?: WalletGeneration | Promise<WalletGeneration>,
): Promise<PasskeyReplacementResult> {
  try {
    return { status: "ok", ...(await registerPasskeySeedRecord(mnemonic, signal, expectedGeneration)) };
  } catch (e) {
    if (isPasskeyAbort(e)) return { status: "aborted" };
    return { status: "error", message: e instanceof Error ? e.message : String(e) };
  }
}

/**
 * Prompt for the passkey and return the mnemonic. With no record on this
 * device it asks for any passkey and restores from its portable copy.
 * `mediation: "conditional"` is passkey autofill (needs a webauthn input).
 */
export async function unlockPasskeySeed(opts?: {
  mediation?: CredentialMediationRequirement;
  signal?: AbortSignal;
}): Promise<string> {
  // Join the wallet lifecycle before the first await: a forget() that lands
  // while the stored formats are being read must cancel this unlock.
  const op = beginVaultOperation("passkey", opts?.signal, true);
  try {
    const [current, old] = await Promise.all([readRaw(PASSKEY_VAULT_KEY, op.signal), readRaw(LEGACY_KEY, op.signal)]);
    op.assertCurrent();
    if (!current && old) throw new PasskeyError("no-vault", "Old passkey backups are not supported. Restore your phrase and create a new vault.");
    const unlocked = await seedVault(fencedStore(op)).unlock({ mediation: opts?.mediation, signal: op.signal });
    const words = unlocked.text();
    unlocked.wipe();
    const generation = await op.generation;
    await walletTransaction<void>("readonly", tx => checkVaultGeneration(tx, "passkey", generation, () => {}), op.signal);
    op.assertCurrent();
    return words;
  } catch (error) {
    throw sdkError(error, op);
  } finally {
    op.dispose();
  }
}

/** Delete this device's passkey records. The passkey itself stays in the password manager. */
export async function forgetPasskeySeed(): Promise<void> {
  invalidateVaultOperations("passkey");
  if (!walletStorageAvailable()) throw new WalletError("wallet_db", "Local passkey storage is unavailable; deletion was not completed.");
  await walletTransaction<void>("readwrite", tx => {
    tx.store.delete(PASSKEY_VAULT_KEY);
    tx.store.delete(LEGACY_KEY);
    tx.store.put(crypto.randomUUID(), domainGenerationKey("passkey"));
  });
}

export async function hasPasskeySeed(): Promise<boolean> {
  const current = await readRaw(PASSKEY_VAULT_KEY);
  return !!(current as PasskeyVaultRecord | undefined)?.passkeys?.length;
}

export async function passkeyInfo(): Promise<PasskeyInfo | null> {
  const current = (await readRaw(PASSKEY_VAULT_KEY)) as PasskeyVaultRecord | undefined;
  const first = current?.passkeys?.[0];
  if (first) {
    return toInfo({
      credentialId: first.credentialId,
      transports: first.transports,
      synced: first.backedUp ?? (first.backupEligible === false ? false : null),
      portable: first.portable === true,
      createdAt: first.createdAt,
      ...(first.aaguid ? { aaguid: first.aaguid } : {}),
    });
  }
  return null;
}

export async function passkeyCapabilities(): Promise<PasskeyCapabilities> {
  const s = await getPasskeySupport(config.rpId ? { rpId: config.rpId } : {});
  return {
    webauthn: s.reason !== "no-webauthn",
    secureContext: s.reason !== "insecure-context",
    rpId: s.rpId,
    localhostUrl: passkeyLocalhostUrl(),
    conditional: s.conditionalGet === true,
    largeBlob: s.largeBlob,
    prf: s.prf,
  };
}

/** WebAuthn RP ID for this page. IP-address origins are not usable. */
export function passkeyRpId(hostname?: string): string | null {
  return config.rpId ?? resolveRpId(hostname);
}

export function passkeyLocalhostUrl(): string | undefined {
  return localhostUrl();
}

/** The user closed the sheet, or the operation was cancelled: stay quiet. */
export function isPasskeyAbort(err: unknown): boolean {
  return isPasskeyCancel(err);
}
