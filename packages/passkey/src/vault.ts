/**
 * A secret (e.g. a recovery phrase) that one or more passkeys can unlock.
 *
 * ```ts
 * const vault = createPasskeyVault({ rpName: "My Wallet" });
 * await vault.protect(mnemonic);                  // one prompt on most platforms
 * const { text } = await vault.unlock();          // one prompt
 * const mnemonic = text();
 * ```
 *
 * Prompts: `protect` shows one sheet when the authenticator returns its PRF
 * secret at creation (Chrome + Google Password Manager, Safari 18 + iCloud),
 * two otherwise (security keys, Windows Hello, some password managers), plus
 * one more with `portable`. `unlock` is always one. `addPasskey` is two or three.
 */

import {
  dataAad,
  FORMAT,
  FORMAT_VERSION,
  keyAad,
  openSecret,
  prfInput,
  sealSecret,
  unwrapDek,
  wrapDek,
  type Sealed,
} from "./crypto";
import { PasskeyError, isPasskeyError, raceAbort, throwIfAborted, toPasskeyError } from "./errors";
import { fromBase64Url, randomBytes, toBase64Url, utf8, wipe } from "./encoding";
import { passkeyProviderName } from "./providers";
import { indexedDbVaultStore, type PasskeyVaultRecord, type PasskeyVaultStore, type StoredPasskey } from "./store";
import { resolveRpId } from "./support";
import {
  assertionJson,
  createCredential,
  getAssertion,
  registrationJson,
  signalUnknownCredential,
  type Assertion,
  type AuthenticationResponseJSON,
  type CreatedCredential,
  type CreationOptionsJSON,
  type RegistrationResponseJSON,
  type RequestOptionsJSON,
} from "./webauthn";

export type PasskeyVaultOptions = {
  /** Shown in the passkey sheet and the password manager, e.g. "Acme Wallet". */
  rpName: string;
  /** WebAuthn RP ID. Default: this page's hostname (`localhost` in development). */
  rpId?: string;
  /** Vault id within the store, for apps with several vaults. Default `"default"`. */
  id?: string;
  /**
   * What the secret is for. It separates keys derived from the same passkey,
   * so two apps (or two features) sharing an RP ID never share a key. Default `"secret"`.
   */
  purpose?: string;
  /** Default: IndexedDB database `z-stack-passkey`. */
  store?: PasskeyVaultStore;
  /**
   * Also save an encrypted copy of the vault inside the passkey (largeBlob)
   * when the authenticator supports it, so the passkey alone can restore the
   * secret on a new device. Costs one extra prompt at registration. Default false.
   * Ignored when the RP ID is a parent domain of the page (every subdomain
   * could read the copy).
   */
  portable?: boolean;
  /** Per-prompt timeout. Default 3 minutes. */
  timeoutMs?: number;
};

export type PasskeyInfo = {
  credentialId: string;
  /** Your label, from `protect`/`addPasskey`/`renamePasskey`. */
  name?: string;
  /** Provider name from the AAGUID ("iCloud Keychain", "1Password", …) when known. */
  provider?: string;
  aaguid?: string;
  transports?: string[];
  /**
   * The passkey syncs to the user's other devices (true), is bound to one
   * device (false), or the authenticator did not say (null). A device-bound
   * passkey is lost with the device: keep another way to recover.
   */
  synced: boolean | null;
  /** The passkey carries an encrypted copy of the vault (see `portable`). */
  portable: boolean;
  createdAt: number;
  lastUsedAt?: number;
};

export type RegisterOptions = {
  /** Account name in the password manager. Default: the vault's saved name, else `rpName`. */
  userName?: string;
  userDisplayName?: string;
  /** Your label for this passkey, shown by `passkeys()`. */
  name?: string;
  /** `"platform"` for this device only, `"cross-platform"` for security keys/phones. Default: let the user choose. */
  attachment?: AuthenticatorAttachment;
  /** WebAuthn hints, e.g. `["client-device"]` or `["security-key"]`. */
  hints?: string[];
  /** Server-issued creation options, when your server also verifies the passkey. */
  server?: CreationOptionsJSON;
  signal?: AbortSignal;
};

export type UnlockOptions = {
  /** `"conditional"`: passkey autofill. Needs an `<input autocomplete="username webauthn">` on the page. */
  mediation?: CredentialMediationRequirement;
  hints?: string[];
  /** Server-issued request options, when your server also verifies the assertion. */
  server?: RequestOptionsJSON;
  signal?: AbortSignal;
};

export type Unlocked = {
  /** The secret. Call `wipe()` when done if you can. */
  bytes: Uint8Array<ArrayBuffer>;
  /** The secret as UTF-8 text. */
  text(): string;
  wipe(): void;
  passkey: PasskeyInfo;
  /** Assertion for your server's verifier (PRF output removed). */
  response: AuthenticationResponseJSON;
  /** The vault was restored from the passkey's portable copy on this device. */
  restored: boolean;
};

export type Registered = {
  passkey: PasskeyInfo;
  /** Registration for your server's verifier (PRF output removed). */
  response: RegistrationResponseJSON;
  /**
   * The vault `protect({ replace: true })` overwrote, when one was saved.
   * Put it back with `import(replaced, { replace: true })` if the new secret
   * must not stay.
   */
  replaced?: PasskeyVaultRecord;
};

export interface PreparedAddPasskey {
  /**
   * Register the new passkey. Call this from a click handler: it starts
   * `credentials.create` before awaiting, so that click's user activation
   * still counts. The unlock already happened in `prepareAddPasskey`.
   */
  register(): Promise<Registered>;
  /** Drop the unlocked data key when `register()` will not be called. */
  cancel(): void;
}

export interface PasskeyVault {
  readonly id: string;
  readonly rpId: string;
  /** A vault is saved in the store. */
  exists(signal?: AbortSignal): Promise<boolean>;
  /** The vault's passkeys (no prompt). */
  passkeys(signal?: AbortSignal): Promise<PasskeyInfo[]>;
  /** Create a passkey and encrypt `secret` for it. Throws `vault-exists` unless `replace`. */
  protect(secret: string | Uint8Array, options?: RegisterOptions & { replace?: boolean }): Promise<Registered>;
  /** Prompt for one of the vault's passkeys and decrypt the secret. */
  unlock(options?: UnlockOptions): Promise<Unlocked>;
  /**
   * Unlock and confirm the vault still holds this secret. Does not create a
   * passkey. Call `register()` from the next click.
   */
  prepareAddPasskey(options?: RegisterOptions & { unlock?: UnlockOptions }): Promise<PreparedAddPasskey>;
  /**
   * `prepareAddPasskey()` then `register()` in one call. Browsers that require
   * a fresh user activation for `credentials.create` need the two calls on
   * separate clicks, because the unlock sheet consumes the first one.
   */
  addPasskey(options?: RegisterOptions & { unlock?: UnlockOptions }): Promise<Registered>;
  /**
   * Remove a passkey from this vault (this device's record only). The passkey
   * stays in the user's password manager; it just can no longer open this vault.
   */
  removePasskey(credentialId: string, signal?: AbortSignal): Promise<void>;
  renamePasskey(credentialId: string, name: string, signal?: AbortSignal): Promise<void>;
  /** Delete the vault record. Passkeys are untouched (see `removePasskey`). */
  forget(signal?: AbortSignal): Promise<void>;
  /** The encrypted record, e.g. to back it up on your server. Safe to store: it holds only ciphertext. */
  export(signal?: AbortSignal): Promise<PasskeyVaultRecord | undefined>;
  /** Save a record from `export()`. Replaces a local vault only if `replace`. */
  import(record: PasskeyVaultRecord, options?: { replace?: boolean; signal?: AbortSignal }): Promise<void>;
}

/** Portable copy written to largeBlob: the record for exactly one passkey. */
type PortableCopy = {
  f: "zspv1";
  id: string;
  rpId: string;
  purpose: string;
  userId: string;
  userName: string;
  data: Sealed;
  salt: string;
  wrapped: Sealed;
};

// WebAuthn allows one pending request per page. A modal ceremony first aborts
// any conditional (autofill) request, so callers never juggle AbortControllers.
let conditional: AbortController | null = null;

function beginCeremony(mediation: CredentialMediationRequirement | undefined, signal?: AbortSignal): {
  signal: AbortSignal;
  done: () => void;
} {
  if (mediation !== "conditional") {
    conditional?.abort(new PasskeyError("aborted", "Replaced by an explicit passkey prompt."));
    conditional = null;
    return { signal: signal ?? new AbortController().signal, done: () => {} };
  }
  conditional?.abort(new PasskeyError("aborted", "Replaced by a newer autofill request."));
  const controller = new AbortController();
  conditional = controller;
  const forward = () => controller.abort(signal?.reason);
  signal?.addEventListener("abort", forward, { once: true });
  if (signal?.aborted) forward();
  return {
    signal: controller.signal,
    done: () => {
      signal?.removeEventListener("abort", forward);
      if (conditional === controller) conditional = null;
    },
  };
}

/**
 * Starting revision for a new record. Random, so a vault another tab
 * recreates never shares a revision with the one this tab read, and a
 * compare-and-swap against the old revision fails instead of overwriting it.
 */
function freshRevision(): number {
  return (crypto.getRandomValues(new Uint32Array(1))[0]! >>> 1) + 1;
}

function info(p: StoredPasskey): PasskeyInfo {
  return {
    credentialId: p.credentialId,
    ...(p.name ? { name: p.name } : {}),
    ...(passkeyProviderName(p.aaguid) ? { provider: passkeyProviderName(p.aaguid) } : {}),
    ...(p.aaguid ? { aaguid: p.aaguid } : {}),
    ...(p.transports ? { transports: p.transports } : {}),
    synced: p.backedUp ?? (p.backupEligible === false ? false : null),
    portable: p.portable === true,
    createdAt: p.createdAt,
    ...(p.lastUsedAt ? { lastUsedAt: p.lastUsedAt } : {}),
  };
}

function toBytes(secret: string | Uint8Array): Uint8Array<ArrayBuffer> {
  if (typeof secret === "string") {
    // Stored exactly as given: whitespace can be part of a secret.
    if (!secret) throw new PasskeyError("unknown", "Nothing to protect: the secret is empty.");
    return utf8(secret);
  }
  if (!secret.byteLength) throw new PasskeyError("unknown", "Nothing to protect: the secret is empty.");
  return new Uint8Array(secret);
}

function unlocked(bytes: Uint8Array<ArrayBuffer>, passkey: PasskeyInfo, credential: PublicKeyCredential, restored: boolean): Unlocked {
  let response: AuthenticationResponseJSON | undefined;
  return {
    bytes,
    text: () => new TextDecoder().decode(bytes),
    wipe: () => wipe(bytes),
    passkey,
    get response() {
      return (response ??= assertionJson(credential));
    },
    restored,
  };
}

function registered(passkey: PasskeyInfo, credential: PublicKeyCredential): Registered {
  let response: RegistrationResponseJSON | undefined;
  return {
    passkey,
    get response() {
      return (response ??= registrationJson(credential));
    },
  };
}

/** Hostname comparison ignores case and a trailing DNS dot (`localhost.`). */
function canonicalHost(value: string | undefined): string | undefined {
  if (!value) return undefined;
  const host = value.trim().toLowerCase().replace(/\.$/, "");
  return host || undefined;
}

function isLocalHost(host: string | undefined): boolean {
  return host === "localhost" || !!host?.endsWith(".localhost");
}

export function createPasskeyVault(options: PasskeyVaultOptions): PasskeyVault {
  const id = options.id ?? "default";
  const purpose = options.purpose ?? "secret";
  const store = options.store ?? indexedDbVaultStore();
  const resolved = options.rpId ?? resolveRpId();
  // A portable copy lives in the authenticator's largeBlob, which every
  // origin under the RP ID can read after user verification. Under a parent
  // RP ID (`example.com` for `wallet.example.com`) a sibling subdomain could
  // read it and, with the PRF output it can also request, open the secret.
  // So portable copies are used only when the RP ID is this page's hostname,
  // never for `localhost` (RP IDs have no port: every local server shares
  // it), and the site must control every subdomain of its hostname.
  const host = canonicalHost(resolved ?? (typeof location === "undefined" ? undefined : location.hostname));
  const pageHost = typeof location === "undefined" ? undefined : canonicalHost(location.hostname);
  const portable =
    options.portable === true &&
    !isLocalHost(host) &&
    (pageHost === undefined || !resolved || pageHost === host);
  if (!options.rpName) throw new PasskeyError("unknown", "createPasskeyVault needs an rpName (shown in the passkey sheet).");

  /**
   * The stored record, unless it is a `forget()` tombstone. Writes still
   * compare against the tombstone's revision, so a write that started before
   * a forget fails instead of bringing the vault back.
   */
  function live(record: PasskeyVaultRecord | undefined): PasskeyVaultRecord | undefined {
    return record && !record.forgotten ? record : undefined;
  }

  function rpId(): string {
    if (resolved) return resolved;
    throw new PasskeyError(
      "rp-id",
      typeof location !== "undefined" && /^(127\.0\.0\.1|\[::1\])$/.test(location.host.replace(/:\d+$/, ""))
        ? "Passkeys need a hostname: open this page on http://localhost instead of an IP address."
        : "This page cannot host passkeys (it needs https or http://localhost).",
    );
  }

  function prfExtensions(extra: Record<string, unknown> = {}) {
    const first = prfInput(purpose);
    return [
      { ...extra, prf: { eval: { first } } },
      { prf: { eval: { first } } },
    ];
  }

  /**
   * Create a credential and obtain its PRF secret, with one follow-up prompt if
   * needed. `navigator.credentials.create` is called before the first await so
   * the click's user activation still counts.
   */
  async function register(
    user: { id: Uint8Array; name: string; displayName: string },
    exclude: StoredPasskey[],
    opts: RegisterOptions,
    signal: AbortSignal,
  ): Promise<{ created: CreatedCredential; prf: Uint8Array<ArrayBuffer> }> {
    const first = prfInput(purpose);
    const created = await createCredential({
      rpId: rpId(),
      rpName: options.rpName,
      user,
      exclude: exclude.map((p) => ({ id: p.credentialId, transports: p.transports })),
      extensions: [
        { credProps: true, prf: { eval: { first } }, ...(portable ? { largeBlob: { support: "preferred" as const } } : {}) },
        { credProps: true, prf: { eval: { first } } },
        { credProps: true, prf: {} },
      ],
      attachment: opts.attachment,
      hints: opts.hints,
      timeoutMs: options.timeoutMs,
      signal,
      server: opts.server,
    });
    const abandon = async (error: unknown): Promise<never> => {
      // The credential is useless without a PRF secret; ask password managers to hide it.
      await signalUnknownCredential(rpId(), created.credentialId);
      throw error;
    };
    if (created.ext.prf) return { created, prf: created.ext.prf };
    if (created.ext.prfEnabled === false) {
      return abandon(
        new PasskeyError(
          "prf-unsupported",
          "This passkey provider cannot protect secrets (no PRF support). Try another provider, a security key, or a passphrase.",
        ),
      );
    }
    // PRF enabled (or not reported) but no output at creation: evaluate it on an assertion.
    let assertion: Assertion;
    try {
      assertion = await getAssertion({
        rpId: rpId(),
        allow: [{ id: created.credentialId, transports: created.transports }],
        extensions: [{ prf: { eval: { first } } }],
        timeoutMs: options.timeoutMs,
        signal,
      });
    } catch (error) {
      return abandon(error);
    }
    if (!assertion.ext.prf) {
      return abandon(
        new PasskeyError(
          "prf-unsupported",
          "This passkey did not return a PRF secret, so it cannot protect secrets. Try another provider, a security key, or a passphrase.",
        ),
      );
    }
    return { created, prf: assertion.ext.prf };
  }

  async function writePortable(record: PasskeyVaultRecord, key: StoredPasskey, signal: AbortSignal): Promise<boolean> {
    const copy: PortableCopy = {
      f: "zspv1",
      id: record.id,
      rpId: record.rpId,
      purpose: record.purpose,
      userId: record.userId,
      userName: record.userName,
      data: record.data,
      salt: key.salt,
      wrapped: key.wrapped,
    };
    try {
      const a = await getAssertion({
        rpId: record.rpId,
        allow: [{ id: key.credentialId, transports: key.transports }],
        extensions: [{ largeBlob: { write: utf8(JSON.stringify(copy)) } }],
        timeoutMs: options.timeoutMs,
        signal,
      });
      return a.ext.largeBlobWritten === true;
    } catch {
      // Written after the vault is committed: a failure or a cancel only
      // means there is no portable copy. The vault works without it.
      return false;
    }
  }

  function readPortable(blob: Uint8Array | undefined, credentialId: string): PasskeyVaultRecord | null {
    if (!blob?.byteLength) return null;
    try {
      const c = JSON.parse(new TextDecoder().decode(blob)) as PortableCopy;
      if (c.f !== "zspv1" || c.id !== id || c.purpose !== purpose || c.rpId !== rpId()) return null;
      const now = Date.now();
      return {
        format: FORMAT,
        version: FORMAT_VERSION,
        id: c.id,
        rpId: c.rpId,
        purpose: c.purpose,
        userId: c.userId,
        userName: c.userName,
        revision: freshRevision(),
        data: c.data,
        passkeys: [{ credentialId, salt: c.salt, wrapped: c.wrapped, portable: true, createdAt: now }],
        createdAt: now,
        updatedAt: now,
      };
    } catch {
      return null;
    }
  }

  /** One prompt: returns the data key, the record it opens, and the passkey used. */
  async function openVault(opts: UnlockOptions): Promise<{
    dek: Uint8Array<ArrayBuffer>;
    record: PasskeyVaultRecord;
    key: StoredPasskey;
    assertion: Assertion;
    restored: boolean;
    /** Revision stored when the vault was read (a tombstone's, when forgotten). */
    base: number | undefined;
  }> {
    const ceremony = beginCeremony(opts.mediation, opts.signal);
    try {
      const raw = await store.get(id, ceremony.signal);
      const local = live(raw);
      if (!local && !portable) {
        throw new PasskeyError("no-vault", "No passkey-protected secret is saved on this device.");
      }
      const assertion = await getAssertion({
        rpId: rpId(),
        // A synced passkey added on another device is not in this record yet.
        // Listing only the local credentials hides it, and a conditional prompt
        // that can already see it still has to read the portable copy.
        allow: portable
          ? undefined
          : local?.passkeys.map((p) => ({ id: p.credentialId, transports: p.transports })),
        extensions: prfExtensions(portable ? { largeBlob: { read: true } } : {}),
        mediation: opts.mediation,
        hints: opts.hints,
        timeoutMs: options.timeoutMs,
        signal: ceremony.signal,
        server: opts.server,
      });
      try {
        // A chooser (especially autofill) may have waited while another tab
        // removed this credential. Check that decision again after approval.
        const current = await store.get(id, ceremony.signal);
        if (local?.revokedCredentialIds?.includes(assertion.credentialId) ||
          current?.revokedCredentialIds?.includes(assertion.credentialId)) {
          throw new PasskeyError("wrong-passkey", "That passkey was removed from this vault. Choose another one.");
        }
      } catch (error) {
        wipe(assertion.ext.prf);
        throw error;
      }
      let record = local;
      let key = local?.passkeys.find((p) => p.credentialId === assertion.credentialId);
      let restored = false;
      if (!key) {
        const copy = readPortable(assertion.ext.largeBlob, assertion.credentialId);
        if (copy && local && copy.userId === local.userId) {
          const incoming = copy.passkeys.find((p) => p.credentialId === assertion.credentialId) ?? copy.passkeys[0];
          const passkeys = incoming && local.passkeys.some((p) => p.credentialId === incoming.credentialId)
            ? local.passkeys
            : [...local.passkeys, ...(incoming ? [{ ...incoming, portable: true as const }] : [])];
          record = {
            ...local,
            revision: (raw?.revision ?? local.revision) + 1,
            updatedAt: Date.now(),
            passkeys,
          };
          key = record.passkeys.find((p) => p.credentialId === assertion.credentialId);
          restored = true;
        } else if (copy && !local) {
          record = copy;
          key = copy.passkeys[0]!;
          restored = true;
        } else if (!local) {
          throw new PasskeyError("no-vault", "This passkey has no secret saved on this device.");
        } else {
          throw new PasskeyError("wrong-passkey", "That passkey does not belong to this vault. Choose another one.");
        }
      }
      if (!key) {
        throw new PasskeyError("wrong-passkey", "That passkey does not belong to this vault. Choose another one.");
      }
      if (!assertion.ext.prf) {
        throw new PasskeyError(
          "prf-unsupported",
          "The passkey did not return its PRF secret. If it was created with user verification, make sure your screen lock is on.",
        );
      }
      const prf = assertion.ext.prf;
      try {
        const dek = await unwrapDek(prf, key.salt, key.wrapped, purpose, keyAad(id, record!.rpId, key.credentialId));
        throwIfAborted(ceremony.signal);
        return { dek, record: record!, key, assertion, restored, base: raw?.revision };
      } finally {
        wipe(prf);
      }
    } finally {
      ceremony.done();
    }
  }

  async function touch(
    record: PasskeyVaultRecord,
    credentialId: string,
    restored: boolean,
    signal?: AbortSignal,
    base?: number,
  ): Promise<void> {
    // lastUsedAt is best effort, but releasing the secret must be ordered
    // against removal, replacement and forget in another tab.
    try {
      const now = Date.now();
      if (restored) {
        // Against what was stored when the vault was read: a forget() since
        // then (a new tombstone) makes this fail.
        await store.put(record, base, signal);
        return;
      }
      const next = {
        ...record,
        revision: record.revision + 1,
        updatedAt: now,
        passkeys: record.passkeys.map((p) => (p.credentialId === credentialId ? { ...p, lastUsedAt: now } : p)),
      };
      await store.put(next, record.revision, signal);
    } catch (error) {
      throwIfAborted(signal);
      // A failed CAS may mean removal committed after openVault's final read.
      // Re-read before releasing any plaintext. If this read fails too, fail
      // closed; unlock's caller wipes both the plaintext and the data key.
      const current = await store.get(id, signal);
      if (current?.revokedCredentialIds?.includes(credentialId)) {
        throw new PasskeyError("wrong-passkey", "That passkey was removed from this vault. Choose another one.");
      }
      // Read-only storage with no intervening change still permits unlocking,
      // including a portable copy that cannot be saved on this device.
      if (current?.revision === (restored ? base : record.revision)) return;
      const active = live(current);
      if (!active || active.userId !== record.userId || active.rpId !== record.rpId ||
        active.purpose !== record.purpose || active.data.iv !== record.data.iv || active.data.ct !== record.data.ct) {
        throw new PasskeyError("conflict", "The vault changed in another tab. Unlock it again.", { cause: error });
      }
      if (!restored && !active.passkeys.some((p) => p.credentialId === credentialId)) {
        throw new PasskeyError("wrong-passkey", "That passkey does not belong to this vault. Choose another one.");
      }
      // Only harmless metadata changed. This read orders unlock before any
      // subsequent removal without overwriting the other tab's newer record.
    }
  }

  const vault: PasskeyVault = {
    id,
    get rpId() {
      return rpId();
    },

    async exists(signal) {
      return !!live(await store.get(id, signal));
    },

    async passkeys(signal) {
      return live(await store.get(id, signal))?.passkeys.map(info) ?? [];
    },

    async protect(secret, opts = {}) {
      const bytes = toBytes(secret);
      const ceremony = beginCeremony(undefined, opts.signal);
      const signal = ceremony.signal;
      let dek: Uint8Array<ArrayBuffer> | null = null;
      let prf: Uint8Array<ArrayBuffer> | null = null;
      let orphan: string | null = null;
      let committed = false;
      let committing = false;
      const work = (async (): Promise<Registered> => {
        // With `replace`, read the old record alongside the prompt instead of
        // before it, so create() runs inside the click's user activation.
        const existingRead = store.get(id, signal);
        existingRead.catch(() => {});
        const userId = opts.server ? fromBase64Url(opts.server.user.id) : randomBytes(16);
        const userName = opts.server?.user.name ?? opts.userName ?? options.rpName;
        // Start the prompt before the storage read settles, so the click's
        // user activation is still valid. Drop the credential if a vault exists.
        const registration = register({ id: userId, name: userName, displayName: opts.userDisplayName ?? userName }, [], opts, signal);
        // The ceremony can reject before asynchronous IndexedDB finishes.
        // Observe it immediately; awaiting the original promise below still
        // propagates the actual failure to the caller and retains cleanup.
        registration.catch(() => {});
        let existing: Awaited<typeof existingRead>;
        try {
          existing = await existingRead;
        } catch (error) {
          const created = await registration.catch(() => null);
          if (created) {
            wipe(created.prf);
            await signalUnknownCredential(rpId(), created.created.credentialId).catch(() => {});
          }
          throw error;
        }
        if (!opts.replace && live(existing)) {
          const created = await registration.catch(() => null);
          if (created) {
            wipe(created.prf);
            await signalUnknownCredential(rpId(), created.created.credentialId).catch(() => {});
          }
          throw new PasskeyError("vault-exists", "A passkey-protected secret is already saved. Pass { replace: true } to overwrite it.");
        }
        const reg = await registration;
        orphan = reg.created.credentialId;
        prf = reg.prf;
        const { created } = reg;
        const sealed = await sealSecret(bytes, dataAad(id, rpId(), purpose), signal);
        dek = sealed.dek;
        const now = Date.now();
        const wrapped = await wrapDek(dek, prf, purpose, keyAad(id, rpId(), created.credentialId));
        const key: StoredPasskey = {
          credentialId: created.credentialId,
          ...(created.transports ? { transports: created.transports } : {}),
          ...wrapped,
          ...(opts.name ? { name: opts.name } : {}),
          ...(created.aaguid ? { aaguid: created.aaguid } : {}),
          ...(created.flags ? { backupEligible: created.flags.backupEligible, backedUp: created.flags.backedUp } : {}),
          createdAt: now,
        };
        const record: PasskeyVaultRecord = {
          format: FORMAT,
          version: FORMAT_VERSION,
          id,
          rpId: rpId(),
          purpose,
          userId: toBase64Url(userId),
          userName,
          revision: existing ? existing.revision + 1 : freshRevision(),
          data: sealed.data,
          passkeys: [key],
          createdAt: now,
          updatedAt: now,
        };
        throwIfAborted(signal);
        // Commit first (as addPasskey does): if another tab replaced or forgot
        // the vault meanwhile, this fails and no portable copy is written.
        // `committing` is set before the write so an abort during the commit
        // waits for the result instead of reporting a cancel of a saved passkey.
        committing = true;
        // No signal: aborting a transaction that has already committed makes
        // the write look cancelled while the passkey is stored.
        await store.put(record, existing?.revision);
        committed = true;
        committing = false;
        orphan = null;
        const previous = live(existing);
        if (portable && created.ext.largeBlobSupported && (await writePortable(record, key, signal))) {
          const marked: PasskeyVaultRecord = {
            ...record,
            revision: record.revision + 1,
            passkeys: [{ ...key, portable: true }],
          };
          // Best effort: the copy exists either way; the flag only tells the UI.
          await store.put(marked, record.revision, signal).then(
            () => (key.portable = true),
            () => undefined,
          );
        }
        const result = registered(info(key), created.credential);
        if (previous) result.replaced = previous;
        return result;
      })();
      try {
        return await raceAbort(signal, work, () => committed, () => committing);
      } catch (error) {
        throw toPasskeyError(error, "create");
      } finally {
        // An abort settles the caller at once; clean up after the work settles.
        void work.catch(() => {}).finally(() => {
          wipe(dek);
          wipe(prf);
          wipe(bytes);
          if (orphan) void signalUnknownCredential(rpId(), orphan);
        });
        ceremony.done();
      }
    },

    async unlock(opts = {}) {
      const work = (async (): Promise<Unlocked> => {
        const opened = await openVault(opts);
        try {
          const secret = await openSecret(opened.dek, opened.record.data, dataAad(id, opened.record.rpId, purpose));
          try {
            throwIfAborted(opts.signal);
            await touch(opened.record, opened.key.credentialId, opened.restored, opts.signal, opened.base);
            throwIfAborted(opts.signal);
          } catch (error) {
            wipe(secret);
            throw error;
          }
          return unlocked(secret, info({ ...opened.key, lastUsedAt: Date.now() }), opened.assertion.credential, opened.restored);
        } finally {
          wipe(opened.dek);
        }
      })();
      try {
        return await raceAbort(opts.signal, work);
      } catch (error) {
        throw toPasskeyError(error, "get");
      } finally {
        // A result the caller abandoned (aborted mid-flight) is wiped.
        void work.then((u) => {
          if (opts.signal?.aborted) u.wipe();
        }, () => {});
      }
    },

    async prepareAddPasskey(opts = {}) {
      let dek: Uint8Array<ArrayBuffer> | null = null;
      const changed = () =>
        new PasskeyError("conflict", "The vault changed in another tab. Unlock it again, then add the passkey.");
      const fail = (error: unknown): never => {
        wipe(dek);
        dek = null;
        throw error;
      };
      let opened: Awaited<ReturnType<typeof openVault>>;
      try {
        opened = await openVault({ ...opts.unlock, signal: opts.unlock?.signal ?? opts.signal, mediation: undefined });
      } catch (error) {
        return fail(error);
      }
      dek = opened.dek;
      try {
        wipe(await openSecret(dek, opened.record.data, dataAad(id, opened.record.rpId, purpose)));
      } catch (error) {
        return fail(error);
      }
      // Confirm the data key still opens the saved record before any create.
      // A passkey made for a secret another tab already replaced cannot decrypt it.
      let record = opened.record;
      if (!opened.restored) {
        const current = live(await store.get(id, opts.signal).catch((error: unknown) => fail(error)));
        if (!current || current.userId !== opened.record.userId || current.rpId !== opened.record.rpId) {
          return fail(changed());
        }
        try {
          wipe(await openSecret(dek, current.data, dataAad(id, current.rpId, purpose)));
        } catch {
          return fail(changed());
        }
        record = current;
      }
      const rp = record.rpId;
      const userName = opts.userName ?? record.userName;
      let started = false;
      let dropped = false;
      const drop = () => {
        if (dropped) return;
        dropped = true;
        wipe(dek);
        dek = null;
      };
      return {
        cancel() {
          if (started) return;
          drop();
        },
        async register() {
          if (dropped) {
            throw new PasskeyError("unknown", "This passkey registration was cancelled.");
          }
          if (started) {
            throw new PasskeyError("unknown", "This passkey registration already started.");
          }
          started = true;
          // First action is credentials.create, before any await, so a click
          // that calls register() still has user activation.
          const ceremony = beginCeremony(undefined, opts.signal);
          const pending = register(
            { id: fromBase64Url(record.userId), name: userName, displayName: opts.userDisplayName ?? userName },
            record.passkeys,
            opts,
            ceremony.signal,
          );
          let prf: Uint8Array<ArrayBuffer> | null = null;
          let orphan: string | null = null;
          let committed = false;
          let committing = false;
          const work = (async (): Promise<Registered> => {
            try {
              const reg = await pending;
              orphan = reg.created.credentialId;
              prf = reg.prf;
              if (!dek) {
                wipe(prf);
                await signalUnknownCredential(rp, orphan);
                throw changed();
              }
              const { created } = reg;
              const now = Date.now();
              const key: StoredPasskey = {
                credentialId: created.credentialId,
                ...(created.transports ? { transports: created.transports } : {}),
                ...(await wrapDek(dek, prf, purpose, keyAad(id, record.rpId, created.credentialId))),
                ...(opts.name ? { name: opts.name } : {}),
                ...(created.aaguid ? { aaguid: created.aaguid } : {}),
                ...(created.flags ? { backupEligible: created.flags.backupEligible, backedUp: created.flags.backedUp } : {}),
                createdAt: now,
              };
              const next: PasskeyVaultRecord = {
                ...record,
                revision: record.revision + 1,
                updatedAt: now,
                passkeys: [...record.passkeys, key],
              };
              throwIfAborted(ceremony.signal);
              committing = true;
              await store.put(next, opened.restored ? opened.base : record.revision);
              committed = true;
              committing = false;
              orphan = null;
              if (portable && created.ext.largeBlobSupported && (await writePortable(next, key, ceremony.signal))) {
                const marked: PasskeyVaultRecord = {
                  ...next,
                  revision: next.revision + 1,
                  passkeys: next.passkeys.map((p) =>
                    p.credentialId === key.credentialId ? { ...p, portable: true } : p,
                  ),
                };
                await store.put(marked, next.revision, ceremony.signal).then(
                  () => (key.portable = true),
                  () => undefined,
                );
              }
              return registered(info(key), created.credential);
            } finally {
              ceremony.done();
            }
          })();
          try {
            return await raceAbort(opts.signal, work, () => committed, () => committing);
          } catch (error) {
            throw toPasskeyError(error, "create");
          } finally {
            drop();
            wipe(prf);
            if (orphan) void signalUnknownCredential(rp, orphan);
          }
        },
      };
    },

    async addPasskey(opts = {}) {
      let prepared: PreparedAddPasskey | undefined;
      try {
        prepared = await this.prepareAddPasskey(opts);
        return await prepared.register();
      } catch (error) {
        throw toPasskeyError(error, "create");
      } finally {
        prepared?.cancel();
      }
    },

    async removePasskey(credentialId, signal) {
      const record = live(await store.get(id, signal));
      if (!record) throw new PasskeyError("no-vault", "No passkey-protected secret is saved on this device.");
      if (!record.passkeys.some((p) => p.credentialId === credentialId)) {
        throw new PasskeyError("wrong-passkey", "That passkey does not belong to this vault.");
      }
      if (record.passkeys.length === 1) {
        throw new PasskeyError("last-passkey", "This is the vault's only passkey. Use forget() to delete the vault instead.");
      }
      await store.put(
        {
          ...record,
          revision: record.revision + 1,
          updatedAt: Date.now(),
          passkeys: record.passkeys.filter((p) => p.credentialId !== credentialId),
          revokedCredentialIds: [...new Set([...(record.revokedCredentialIds ?? []), credentialId])],
        },
        record.revision,
        signal,
      );
    },

    async renamePasskey(credentialId, name, signal) {
      const record = live(await store.get(id, signal));
      if (!record) throw new PasskeyError("no-vault", "No passkey-protected secret is saved on this device.");
      if (!record.passkeys.some((p) => p.credentialId === credentialId)) {
        throw new PasskeyError("wrong-passkey", "That passkey does not belong to this vault.");
      }
      const label = name.trim();
      await store.put(
        {
          ...record,
          revision: record.revision + 1,
          updatedAt: Date.now(),
          passkeys: record.passkeys.map((p) => {
            if (p.credentialId !== credentialId) return p;
            const { name: _old, ...rest } = p;
            return label ? { ...rest, name: label } : rest;
          }),
        },
        record.revision,
        signal,
      );
    },

    async forget(signal) {
      // A tombstone, not a delete: a write that read the vault (or its
      // absence) before this forget then fails its compare-and-swap instead
      // of recreating what the user just removed.
      for (let attempt = 0; ; attempt++) {
        const current = await store.get(id, signal);
        const now = Date.now();
        const tombstone: PasskeyVaultRecord = {
          format: FORMAT,
          version: FORMAT_VERSION,
          id,
          rpId: current?.rpId ?? "",
          purpose,
          userId: "",
          userName: "",
          revision: current ? current.revision + 1 : freshRevision(),
          data: { iv: "", ct: "" },
          passkeys: [],
          createdAt: now,
          updatedAt: now,
          forgotten: true,
        };
        try {
          await store.put(tombstone, current?.revision, signal);
          return;
        } catch (error) {
          if (attempt >= 4 || !isPasskeyError(error) || error.code !== "conflict") throw error;
        }
      }
    },

    async export(signal) {
      return live(await store.get(id, signal));
    },

    async import(record, opts = {}) {
      if (record.format !== FORMAT || record.version !== FORMAT_VERSION) {
        throw new PasskeyError("unknown", "Not a passkey vault record (or a newer format).");
      }
      if (record.forgotten) {
        throw new PasskeyError("no-vault", "This record marks a forgotten vault and cannot restore a secret.");
      }
      if (record.id !== id || record.purpose !== purpose || record.rpId !== rpId()) {
        throw new PasskeyError("wrong-passkey", "That record belongs to a different vault, purpose or RP ID.");
      }
      const existing = await store.get(id, opts.signal);
      if (live(existing) && !opts.replace) {
        throw new PasskeyError("vault-exists", "A vault is already saved. Pass { replace: true } to overwrite it.");
      }
      await store.put(
        { ...record, revision: existing ? existing.revision + 1 : freshRevision() },
        existing?.revision,
        opts.signal,
      );
    },
  };
  return vault;
}

export { isPasskeyError };
