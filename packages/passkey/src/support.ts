/**
 * What this browser can do with passkeys, from the browser itself (no
 * user-agent sniffing). `getClientCapabilities()` reports extensions as
 * `extension:prf` / `extension:largeBlob`; older code that reads `caps.prf`
 * always sees `undefined`.
 *
 * Browser support for the PRF extension does not mean every authenticator has
 * it: the answer for a specific passkey is only known after `create()`. The
 * vault handles that; `prf` here is only a hint for whether to offer passkeys.
 */

export type PasskeySupport = {
  /** Passkeys can be used on this page right now. */
  available: boolean;
  /** Why not, when `available` is false. */
  reason?: "no-webauthn" | "insecure-context" | "invalid-rp-id";
  /** The RP ID this page would use, or null (IP-address origins cannot host passkeys). */
  rpId: string | null;
  /** The same page on `localhost`, when opened on `127.0.0.1`/`[::1]` (passkeys need a hostname). */
  localhostUrl?: string;
  /** A platform authenticator (Touch ID, Windows Hello, Android) is available. */
  platformAuthenticator: boolean | null;
  /** Passkey autofill (`mediation: "conditional"`) is available. */
  conditionalGet: boolean | null;
  /** Silent passkey creation after a password sign-in. */
  conditionalCreate: boolean | null;
  /** Phone-as-authenticator via QR code. */
  hybridTransport: boolean | null;
  /** The browser implements the PRF extension (the vault needs it). */
  prf: boolean | null;
  /** The browser implements the largeBlob extension (portable vault copies). */
  largeBlob: boolean | null;
  /** Related Origin Requests (`/.well-known/webauthn`) for an RP ID that is not a suffix of this host. */
  relatedOrigins: boolean | null;
  /** WebAuthn Signal API, used to keep password managers in sync. */
  signals: { unknownCredential: boolean; allAcceptedCredentials: boolean; currentUserDetails: boolean };
};

/**
 * The WebAuthn RP ID for a hostname: the hostname itself, `localhost` for
 * `localhost`/`*.localhost`, and null for IP addresses (WebAuthn forbids them).
 */
export function resolveRpId(hostname = pageHostname()): string | null {
  const host = hostname.replace(/^\[|\]$/g, "").toLowerCase();
  if (!host) return null;
  if (host === "localhost" || host.endsWith(".localhost")) return "localhost";
  if (/^\d{1,3}(\.\d{1,3}){3}$/.test(host) || host.includes(":")) return null;
  return host;
}

/** On `127.0.0.1` or `[::1]`, the same URL on `localhost`, where passkeys work. */
export function localhostUrl(): string | undefined {
  if (typeof location === "undefined") return undefined;
  const host = location.hostname;
  if (host !== "127.0.0.1" && host !== "[::1]" && host !== "::1") return undefined;
  return `${location.protocol}//localhost${location.port ? `:${location.port}` : ""}${location.pathname}${location.search}${location.hash}`;
}

function pageHostname(): string {
  return typeof location === "undefined" ? "" : location.hostname;
}

type ClientCapabilities = Record<string, boolean | undefined>;
type PublicKeyCredentialStatics = {
  getClientCapabilities?: () => Promise<ClientCapabilities>;
  isUserVerifyingPlatformAuthenticatorAvailable?: () => Promise<boolean>;
  isConditionalMediationAvailable?: () => Promise<boolean>;
  signalUnknownCredential?: unknown;
  signalAllAcceptedCredentials?: unknown;
  signalCurrentUserDetails?: unknown;
};

function statics(): PublicKeyCredentialStatics | null {
  return typeof PublicKeyCredential === "undefined" ? null : (PublicKeyCredential as unknown as PublicKeyCredentialStatics);
}

async function ask(fn: (() => Promise<boolean>) | undefined, self: unknown): Promise<boolean | null> {
  if (typeof fn !== "function") return null;
  try {
    return await fn.call(self);
  } catch {
    return null;
  }
}

/** Probe passkey support. Cheap and safe to call on page load; never prompts. */
export async function getPasskeySupport(options: { rpId?: string } = {}): Promise<PasskeySupport> {
  const pkc = statics();
  const rpId = options.rpId ?? resolveRpId();
  const secure = typeof isSecureContext === "undefined" ? false : isSecureContext;
  const signals = {
    unknownCredential: typeof pkc?.signalUnknownCredential === "function",
    allAcceptedCredentials: typeof pkc?.signalAllAcceptedCredentials === "function",
    currentUserDetails: typeof pkc?.signalCurrentUserDetails === "function",
  };
  const base: PasskeySupport = {
    available: false,
    rpId,
    localhostUrl: localhostUrl(),
    platformAuthenticator: null,
    conditionalGet: null,
    conditionalCreate: null,
    hybridTransport: null,
    prf: null,
    largeBlob: null,
    relatedOrigins: null,
    signals,
  };
  if (!pkc) return { ...base, reason: "no-webauthn" };

  let caps: ClientCapabilities = {};
  if (typeof pkc.getClientCapabilities === "function") {
    try {
      caps = (await pkc.getClientCapabilities.call(PublicKeyCredential)) ?? {};
    } catch {
      caps = {};
    }
  }
  const flag = (key: string): boolean | null => (typeof caps[key] === "boolean" ? (caps[key] as boolean) : null);
  const support: PasskeySupport = {
    ...base,
    platformAuthenticator:
      flag("passkeyPlatformAuthenticator") ??
      flag("userVerifyingPlatformAuthenticator") ??
      (await ask(pkc.isUserVerifyingPlatformAuthenticatorAvailable, PublicKeyCredential)),
    conditionalGet: flag("conditionalGet") ?? (await ask(pkc.isConditionalMediationAvailable, PublicKeyCredential)),
    conditionalCreate: flag("conditionalCreate"),
    hybridTransport: flag("hybridTransport"),
    prf: flag("extension:prf"),
    largeBlob: flag("extension:largeBlob"),
    relatedOrigins: flag("relatedOrigins"),
  };
  if (!secure) return { ...support, reason: "insecure-context" };
  if (!rpId) return { ...support, reason: "invalid-rp-id" };
  return { ...support, available: true };
}
