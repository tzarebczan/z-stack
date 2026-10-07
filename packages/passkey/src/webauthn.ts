/**
 * The two WebAuthn ceremonies, called directly (no client wrapper library).
 *
 * Choices that are easy to get wrong, made once here:
 * - `userVerification: "required"` everywhere. The PRF secret an authenticator
 *   returns depends on whether the user was verified, so mixing "preferred" and
 *   "required" can derive two different keys from one passkey.
 * - Discoverable credentials (`residentKey: "required"`) so a new device can
 *   unlock without knowing credential IDs, and no `authenticatorAttachment` by
 *   default so password managers and security keys stay in the chooser.
 * - Server JSON options go through `parse*OptionsFromJSON` (with a fallback for
 *   older Safari), and responses leave through {@link registrationJson} /
 *   {@link assertionJson}, which strip PRF and largeBlob outputs. Plain
 *   `credential.toJSON()` would send the PRF secret to your server.
 */

import { PasskeyError, raceAbort, throwIfAborted, toPasskeyError } from "./errors";
import { bytesOf, fromBase64Url, randomBytes, toBase64Url } from "./encoding";

export type CredentialDescriptor = { id: string; transports?: string[] };

type Extensions = {
  credProps?: boolean;
  prf?: { eval?: { first: BufferSource } } | Record<string, never>;
  largeBlob?: { support?: "preferred" | "required"; read?: boolean; write?: BufferSource };
};

export type RegistrationResponseJSON = {
  id: string;
  rawId: string;
  type: "public-key";
  authenticatorAttachment?: string | null;
  response: {
    clientDataJSON: string;
    attestationObject: string;
    authenticatorData?: string;
    transports?: string[];
    publicKey?: string;
    publicKeyAlgorithm?: number;
  };
  clientExtensionResults: Record<string, unknown>;
};

export type AuthenticationResponseJSON = {
  id: string;
  rawId: string;
  type: "public-key";
  authenticatorAttachment?: string | null;
  response: {
    clientDataJSON: string;
    authenticatorData: string;
    signature: string;
    userHandle?: string;
  };
  clientExtensionResults: Record<string, unknown>;
};

/** Server-issued options (SimpleWebAuthn, py_webauthn, …) in WebAuthn JSON form. */
export type CreationOptionsJSON = {
  challenge: string;
  rp: { id?: string; name: string };
  user: { id: string; name: string; displayName: string };
  pubKeyCredParams?: Array<{ type: "public-key"; alg: number }>;
  timeout?: number;
  excludeCredentials?: Array<{ type: "public-key"; id: string; transports?: string[] }>;
  authenticatorSelection?: Record<string, unknown>;
  attestation?: string;
  hints?: string[];
  extensions?: Record<string, unknown>;
};

export type RequestOptionsJSON = {
  challenge: string;
  rpId?: string;
  timeout?: number;
  allowCredentials?: Array<{ type: "public-key"; id: string; transports?: string[] }>;
  userVerification?: string;
  hints?: string[];
  extensions?: Record<string, unknown>;
};

export type AuthenticatorFlags = {
  userVerified: boolean;
  /** The credential can be synced to other devices (BE). */
  backupEligible: boolean;
  /** The credential is currently synced (BS). */
  backedUp: boolean;
};

export type CreatedCredential = {
  credential: PublicKeyCredential;
  credentialId: string;
  transports?: string[];
  aaguid?: string;
  flags?: AuthenticatorFlags;
  ext: ExtensionOutputs;
};

export type Assertion = {
  credential: PublicKeyCredential;
  credentialId: string;
  userHandle?: string;
  flags?: AuthenticatorFlags;
  ext: ExtensionOutputs;
};

export type ExtensionOutputs = {
  prfEnabled?: boolean;
  prf?: Uint8Array<ArrayBuffer>;
  largeBlobSupported?: boolean;
  largeBlob?: Uint8Array<ArrayBuffer>;
  largeBlobWritten?: boolean;
  discoverable?: boolean;
};

const DEFAULT_TIMEOUT_MS = 180_000;

function requireWebAuthn(): void {
  if (typeof navigator === "undefined" || !navigator.credentials || typeof PublicKeyCredential === "undefined") {
    throw new PasskeyError("unsupported", "This browser does not support passkeys (WebAuthn).");
  }
}

function sameCredentialId(a: BufferSource, b: BufferSource): boolean {
  const left = bytesOf(a);
  const right = bytesOf(b);
  if (!left || !right || left.byteLength !== right.byteLength) return false;
  return left.every((byte, index) => byte === right[index]);
}

function descriptor(d: CredentialDescriptor): PublicKeyCredentialDescriptor {
  return {
    type: "public-key",
    id: fromBase64Url(d.id),
    ...(d.transports?.length ? { transports: d.transports as AuthenticatorTransport[] } : {}),
  };
}

function readExtensions(credential: PublicKeyCredential): ExtensionOutputs {
  let raw: Record<string, unknown> = {};
  try {
    raw = (credential.getClientExtensionResults() as Record<string, unknown>) ?? {};
  } catch {
    raw = {};
  }
  const prf = raw.prf as { enabled?: boolean; results?: { first?: BufferSource } } | undefined;
  const largeBlob = raw.largeBlob as { supported?: boolean; blob?: BufferSource; written?: boolean } | undefined;
  const credProps = raw.credProps as { rk?: boolean } | undefined;
  return {
    prfEnabled: prf?.enabled,
    prf: bytesOf(prf?.results?.first) ?? undefined,
    largeBlobSupported: largeBlob?.supported,
    largeBlob: bytesOf(largeBlob?.blob) ?? undefined,
    largeBlobWritten: largeBlob?.written,
    discoverable: credProps?.rk,
  };
}

/** Authenticator data flags, and the AAGUID when attested credential data is present. */
export function parseAuthenticatorData(data: ArrayBuffer | Uint8Array | null | undefined): {
  flags: AuthenticatorFlags;
  aaguid?: string;
} | null {
  if (!data) return null;
  const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
  if (bytes.length < 37) return null;
  const f = bytes[32]!;
  const flags = { userVerified: (f & 0x04) !== 0, backupEligible: (f & 0x08) !== 0, backedUp: (f & 0x10) !== 0 };
  let aaguid: string | undefined;
  if ((f & 0x40) !== 0 && bytes.length >= 53) {
    const hex = Array.from(bytes.slice(37, 53), (b) => b.toString(16).padStart(2, "0")).join("");
    if (!/^0+$/.test(hex)) {
      aaguid = `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
    }
  }
  return { flags, aaguid };
}

function isExtensionRejection(error: unknown): boolean {
  const name = typeof error === "object" && error !== null && "name" in error ? String((error as { name: unknown }).name) : "";
  if (name === "NotAllowedError" || name === "AbortError" || name === "InvalidStateError" || name === "SecurityError") return false;
  if (name === "NotSupportedError" || name === "TypeError") return true;
  const message = error instanceof Error ? error.message : String(error);
  return /extension|prf|largeblob|hmac-secret/i.test(message);
}

/** Parse server creation options (native parser when present, manual fallback otherwise). */
export function parseCreationOptions(json: CreationOptionsJSON): PublicKeyCredentialCreationOptions {
  const pkc = PublicKeyCredential as unknown as {
    parseCreationOptionsFromJSON?: (j: unknown) => PublicKeyCredentialCreationOptions;
  };
  const { extensions: _dropped, ...rest } = json;
  if (typeof pkc.parseCreationOptionsFromJSON === "function") return pkc.parseCreationOptionsFromJSON(rest);
  return {
    ...(rest as unknown as PublicKeyCredentialCreationOptions),
    challenge: fromBase64Url(json.challenge),
    user: { ...json.user, id: fromBase64Url(json.user.id) },
    pubKeyCredParams: json.pubKeyCredParams ?? defaultAlgorithms(),
    excludeCredentials: json.excludeCredentials?.map((c) => descriptor(c)),
  } as PublicKeyCredentialCreationOptions;
}

export function parseRequestOptions(json: RequestOptionsJSON): PublicKeyCredentialRequestOptions {
  const pkc = PublicKeyCredential as unknown as {
    parseRequestOptionsFromJSON?: (j: unknown) => PublicKeyCredentialRequestOptions;
  };
  const { extensions: _dropped, ...rest } = json;
  if (typeof pkc.parseRequestOptionsFromJSON === "function") return pkc.parseRequestOptionsFromJSON(rest);
  return {
    ...(rest as unknown as PublicKeyCredentialRequestOptions),
    challenge: fromBase64Url(json.challenge),
    allowCredentials: json.allowCredentials?.map((c) => descriptor(c)),
  } as PublicKeyCredentialRequestOptions;
}

function defaultAlgorithms(): PublicKeyCredentialParameters[] {
  // ES256 and RS256 cover every platform authenticator; EdDSA for some security keys.
  return [
    { type: "public-key", alg: -7 },
    { type: "public-key", alg: -257 },
    { type: "public-key", alg: -8 },
  ];
}

export type CreateArgs = {
  rpId: string;
  rpName: string;
  user: { id: Uint8Array; name: string; displayName: string };
  exclude?: CredentialDescriptor[];
  /** Tried in order; the next one is used only if the provider rejects an extension. */
  extensions: Extensions[];
  attachment?: AuthenticatorAttachment;
  hints?: string[];
  timeoutMs?: number;
  signal?: AbortSignal;
  /** Server-issued options. Their challenge, RP and user win; extensions are ours. */
  server?: CreationOptionsJSON;
};

export async function createCredential(args: CreateArgs): Promise<CreatedCredential> {
  requireWebAuthn();
  const base: PublicKeyCredentialCreationOptions = args.server
    ? parseCreationOptions(args.server)
    : {
        rp: { id: args.rpId, name: args.rpName },
        user: { id: args.user.id as BufferSource, name: args.user.name, displayName: args.user.displayName },
        challenge: randomBytes(32),
        pubKeyCredParams: defaultAlgorithms(),
        attestation: "none",
      };
  if (base.rp.id && base.rp.id !== args.rpId) {
    throw new PasskeyError("rp-id", `server RP ID "${base.rp.id}" does not match the vault's "${args.rpId}"`);
  }
  const exclude = [...(base.excludeCredentials ?? []), ...(args.exclude ?? []).map(descriptor)];
  let last: unknown;
  for (const extensions of args.extensions) {
    throwIfAborted(args.signal);
    const publicKey = {
      ...base,
      rp: { ...base.rp, id: args.rpId },
      timeout: args.timeoutMs ?? base.timeout ?? DEFAULT_TIMEOUT_MS,
      excludeCredentials: exclude,
      authenticatorSelection: {
        ...(base.authenticatorSelection ?? {}),
        residentKey: "required",
        requireResidentKey: true,
        userVerification: "required",
        ...(args.attachment ? { authenticatorAttachment: args.attachment } : {}),
      },
      ...(args.hints?.length ? { hints: args.hints } : {}),
      extensions: extensions as AuthenticationExtensionsClientInputs,
    } as PublicKeyCredentialCreationOptions;
    try {
      const credential = (await raceAbort(args.signal, navigator.credentials.create({ publicKey, signal: args.signal }))) as PublicKeyCredential | null;
      if (!credential || credential.type !== "public-key") {
        throw new PasskeyError("unknown", "The browser returned no passkey.");
      }
      const response = credential.response as AuthenticatorAttestationResponse;
      const transports = typeof response.getTransports === "function" ? response.getTransports() : undefined;
      const authData = typeof response.getAuthenticatorData === "function" ? parseAuthenticatorData(response.getAuthenticatorData()) : null;
      return {
        credential,
        credentialId: toBase64Url(credential.rawId),
        transports: transports?.length ? transports : undefined,
        aaguid: authData?.aaguid,
        flags: authData?.flags,
        ext: readExtensions(credential),
      };
    } catch (error) {
      if (args.signal?.aborted) throw toPasskeyError(args.signal.reason ?? error, "create");
      last = error;
      if (!isExtensionRejection(error)) throw toPasskeyError(error, "create");
    }
  }
  throw toPasskeyError(last, "create");
}

export type GetArgs = {
  rpId: string;
  /** Empty or omitted: discoverable (the user picks any passkey for this RP). */
  allow?: CredentialDescriptor[];
  extensions: Extensions[];
  mediation?: CredentialMediationRequirement;
  hints?: string[];
  timeoutMs?: number;
  signal?: AbortSignal;
  server?: RequestOptionsJSON;
};

export async function getAssertion(args: GetArgs): Promise<Assertion> {
  requireWebAuthn();
  const base: PublicKeyCredentialRequestOptions = args.server
    ? parseRequestOptions(args.server)
    : { challenge: randomBytes(32) };
  if (base.rpId && base.rpId !== args.rpId) {
    throw new PasskeyError("rp-id", `server RP ID "${base.rpId}" does not match the vault's "${args.rpId}"`);
  }
  const conditional = args.mediation === "conditional";
  const serverAllow = base.allowCredentials ?? [];
  const localAllow = (args.allow ?? []).map(descriptor);
  const allow = conditional
    ? []
    : !serverAllow.length
      ? localAllow
      : !localAllow.length
        ? serverAllow
        : localAllow.filter((local) => serverAllow.some((server) => sameCredentialId(server.id, local.id)));
  if (!conditional && serverAllow.length && localAllow.length && !allow.length) {
    throw new PasskeyError("cancelled", "None of the saved passkeys are accepted by the server.");
  }
  let last: unknown;
  for (const extensions of args.extensions) {
    throwIfAborted(args.signal);
    const publicKey = {
      ...base,
      rpId: args.rpId,
      userVerification: "required",
      ...(conditional ? {} : { timeout: args.timeoutMs ?? base.timeout ?? DEFAULT_TIMEOUT_MS }),
      ...(allow.length ? { allowCredentials: allow } : { allowCredentials: [] }),
      ...(args.hints?.length ? { hints: args.hints } : {}),
      extensions: extensions as AuthenticationExtensionsClientInputs,
    } as PublicKeyCredentialRequestOptions;
    try {
      const credential = (await raceAbort(
        args.signal,
        navigator.credentials.get({
          publicKey,
          signal: args.signal,
          ...(args.mediation ? { mediation: args.mediation } : {}),
        }),
      )) as PublicKeyCredential | null;
      if (!credential || credential.type !== "public-key") {
        throw new PasskeyError("cancelled", "No passkey was chosen.");
      }
      const response = credential.response as AuthenticatorAssertionResponse;
      const authData = parseAuthenticatorData(response.authenticatorData);
      return {
        credential,
        credentialId: toBase64Url(credential.rawId),
        userHandle: response.userHandle && response.userHandle.byteLength ? toBase64Url(response.userHandle) : undefined,
        flags: authData?.flags,
        ext: readExtensions(credential),
      };
    } catch (error) {
      if (args.signal?.aborted) throw toPasskeyError(args.signal.reason ?? error, "get");
      last = error;
      if (!isExtensionRejection(error)) throw toPasskeyError(error, "get");
    }
  }
  throw toPasskeyError(last, "get");
}

/** Extension outputs that must never leave the browser (they are key material). */
function safeExtensionResults(credential: PublicKeyCredential): Record<string, unknown> {
  let raw: Record<string, unknown> = {};
  try {
    raw = (credential.getClientExtensionResults() as Record<string, unknown>) ?? {};
  } catch {
    raw = {};
  }
  const out: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(raw)) {
    if (key === "prf") {
      const enabled = (value as { enabled?: boolean })?.enabled;
      if (enabled !== undefined) out.prf = { enabled };
    } else if (key === "largeBlob") {
      const { supported, written } = (value ?? {}) as { supported?: boolean; written?: boolean };
      out.largeBlob = { ...(supported !== undefined ? { supported } : {}), ...(written !== undefined ? { written } : {}) };
    } else {
      out[key] = value;
    }
  }
  return out;
}

/** Registration response for your server's verifier, without PRF/largeBlob outputs. */
export function registrationJson(credential: PublicKeyCredential): RegistrationResponseJSON {
  const r = credential.response as AuthenticatorAttestationResponse;
  const publicKey = typeof r.getPublicKey === "function" ? r.getPublicKey() : null;
  const authData = typeof r.getAuthenticatorData === "function" ? r.getAuthenticatorData() : null;
  return {
    id: toBase64Url(credential.rawId),
    rawId: toBase64Url(credential.rawId),
    type: "public-key",
    authenticatorAttachment: credential.authenticatorAttachment ?? null,
    response: {
      clientDataJSON: toBase64Url(r.clientDataJSON),
      attestationObject: toBase64Url(r.attestationObject),
      ...(authData ? { authenticatorData: toBase64Url(authData) } : {}),
      ...(typeof r.getTransports === "function" ? { transports: r.getTransports() } : {}),
      ...(publicKey ? { publicKey: toBase64Url(publicKey) } : {}),
      ...(typeof r.getPublicKeyAlgorithm === "function" ? { publicKeyAlgorithm: r.getPublicKeyAlgorithm() } : {}),
    },
    clientExtensionResults: safeExtensionResults(credential),
  };
}

/** Assertion for your server's verifier, without PRF/largeBlob outputs. */
export function assertionJson(credential: PublicKeyCredential): AuthenticationResponseJSON {
  const r = credential.response as AuthenticatorAssertionResponse;
  return {
    id: toBase64Url(credential.rawId),
    rawId: toBase64Url(credential.rawId),
    type: "public-key",
    authenticatorAttachment: credential.authenticatorAttachment ?? null,
    response: {
      clientDataJSON: toBase64Url(r.clientDataJSON),
      authenticatorData: toBase64Url(r.authenticatorData),
      signature: toBase64Url(r.signature),
      ...(r.userHandle && r.userHandle.byteLength ? { userHandle: toBase64Url(r.userHandle) } : {}),
    },
    clientExtensionResults: safeExtensionResults(credential),
  };
}

type Signals = {
  signalUnknownCredential?: (o: { rpId: string; credentialId: string }) => Promise<void>;
  signalAllAcceptedCredentials?: (o: { rpId: string; userId: string; allAcceptedCredentialIds: string[] }) => Promise<void>;
  signalCurrentUserDetails?: (o: { rpId: string; userId: string; name: string; displayName: string }) => Promise<void>;
};

function signals(): Signals | null {
  return typeof PublicKeyCredential === "undefined" ? null : (PublicKeyCredential as unknown as Signals);
}

/**
 * Ask password managers to hide a credential the app will never accept (for
 * example one abandoned right after `create()`). No-op where unsupported.
 */
export async function signalUnknownCredential(rpId: string, credentialId: string): Promise<boolean> {
  const fn = signals()?.signalUnknownCredential;
  if (typeof fn !== "function") return false;
  try {
    await fn.call(PublicKeyCredential, { rpId, credentialId });
    return true;
  } catch {
    return false;
  }
}

/**
 * Tell password managers the complete list of passkeys a user may use. Only
 * call this from a source of truth (a server that knows every device's
 * passkeys): providers hide anything not listed, on every synced device.
 */
export async function signalAllAcceptedCredentials(rpId: string, userId: string, credentialIds: string[]): Promise<boolean> {
  const fn = signals()?.signalAllAcceptedCredentials;
  if (typeof fn !== "function") return false;
  try {
    await fn.call(PublicKeyCredential, { rpId, userId, allAcceptedCredentialIds: credentialIds });
    return true;
  } catch {
    return false;
  }
}

/** Update the account name shown in password managers. No-op where unsupported. */
export async function signalCurrentUserDetails(rpId: string, userId: string, name: string, displayName = name): Promise<boolean> {
  const fn = signals()?.signalCurrentUserDetails;
  if (typeof fn !== "function") return false;
  try {
    await fn.call(PublicKeyCredential, { rpId, userId, name, displayName });
    return true;
  } catch {
    return false;
  }
}
