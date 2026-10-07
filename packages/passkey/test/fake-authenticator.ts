/**
 * A scriptable WebAuthn authenticator for Node tests. It models the provider
 * behaviours that matter to the vault: PRF at creation or only on assertion,
 * no PRF, extension rejection, largeBlob, cancel, abort and excludeCredentials.
 */

export type ProviderBehaviour = {
  name: string;
  aaguid?: string;
  /** Returns PRF results from create() (Chrome + GPM, Safari 18 + iCloud). */
  prfAtCreate?: boolean;
  /** Supports PRF at all. */
  prf?: boolean;
  /** Supports largeBlob. */
  largeBlob?: boolean;
  /** Reports the credential as synced (BE+BS flags). */
  synced?: boolean;
  /** Throw NotSupportedError when these extension keys are present. */
  rejects?: string[];
};

type Credential = {
  id: Uint8Array;
  rpId: string;
  userId: Uint8Array;
  prfKey: CryptoKey;
  blob?: Uint8Array;
  provider: ProviderBehaviour;
};

type Pending = { resolve: () => void; reject: (e: unknown) => void };

function b64u(bytes: ArrayBuffer | Uint8Array): string {
  const v = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  return Buffer.from(v).toString("base64url");
}

function bytes(src: BufferSource): Uint8Array {
  return src instanceof ArrayBuffer ? new Uint8Array(src) : new Uint8Array(src.buffer, src.byteOffset, src.byteLength);
}

function dom(name: string, message = name): DOMException {
  return new DOMException(message, name);
}

export function fakeWebAuthn() {
  const credentials: Credential[] = [];
  const log: string[] = [];
  const signalled: Array<{ kind: string; arg: unknown }> = [];
  let provider: ProviderBehaviour = { name: "gpm", prf: true, prfAtCreate: true, synced: true, aaguid: "ea9b8d66-4d01-1d21-3ce4-b6b48cb575d4" };
  let cancelNext: "create" | "get" | null = null;
  let choose: ((ids: string[]) => string | undefined) | null = null;
  const conditionalWaiters: Pending[] = [];
  const extensionLog: Array<Record<string, unknown>> = [];

  async function prf(cred: Credential, input: BufferSource): Promise<ArrayBuffer> {
    const salt = new Uint8Array(await crypto.subtle.digest("SHA-256", Buffer.concat([Buffer.from("WebAuthn PRF\0"), bytes(input)])));
    return crypto.subtle.sign("HMAC", cred.prfKey, salt);
  }

  function checkRejects(ext: Record<string, unknown>) {
    for (const key of provider.rejects ?? []) {
      if (key in ext) throw dom("NotSupportedError", `extension ${key} not supported`);
    }
  }

  function authData(cred: Credential, attested: boolean): ArrayBuffer {
    const flags = 0x01 | 0x04 | (cred.provider.synced ? 0x18 : 0) | (attested ? 0x40 : 0);
    const out = new Uint8Array(attested ? 55 + cred.id.length : 37);
    out[32] = flags;
    if (attested) {
      const hex = (cred.provider.aaguid ?? "00000000-0000-0000-0000-000000000000").replace(/-/g, "");
      out.set(Buffer.from(hex, "hex"), 37);
      out[53] = cred.id.length >> 8;
      out[54] = cred.id.length & 0xff;
      out.set(cred.id, 55);
    }
    return out.buffer;
  }

  function abortable<T>(signal: AbortSignal | undefined, run: () => Promise<T>): Promise<T> {
    if (signal?.aborted) return Promise.reject(dom("AbortError"));
    return new Promise<T>((resolve, reject) => {
      const onAbort = () => reject(dom("AbortError"));
      signal?.addEventListener("abort", onAbort, { once: true });
      run().then(resolve, reject).finally(() => signal?.removeEventListener("abort", onAbort));
    });
  }

  const container = {
    async create(options: CredentialCreationOptions): Promise<unknown> {
      const pk = options.publicKey!;
      return abortable(options.signal ?? undefined, async () => {
        const ext = (pk.extensions ?? {}) as Record<string, any>;
        extensionLog.push({ op: "create", ...ext });
        log.push(`create:${provider.name}`);
        if (cancelNext === "create") {
          cancelNext = null;
          throw dom("NotAllowedError");
        }
        checkRejects(ext);
        const rpId = pk.rp.id!;
        for (const ex of pk.excludeCredentials ?? []) {
          const hit = credentials.find((c) => c.rpId === rpId && b64u(c.id) === b64u(bytes(ex.id)) && c.provider.name === provider.name);
          if (hit) throw dom("InvalidStateError", "credential excluded");
        }
        const cred: Credential = {
          id: crypto.getRandomValues(new Uint8Array(16)),
          rpId,
          userId: bytes(pk.user.id),
          prfKey: await crypto.subtle.generateKey({ name: "HMAC", hash: "SHA-256" }, false, ["sign"]),
          provider,
        };
        credentials.push(cred);
        const results: Record<string, unknown> = {};
        if (ext.credProps) results.credProps = { rk: true };
        if (ext.prf) {
          results.prf = { enabled: !!provider.prf };
          if (provider.prf && provider.prfAtCreate && ext.prf.eval?.first) {
            (results.prf as any).results = { first: await prf(cred, ext.prf.eval.first) };
          }
        }
        if (ext.largeBlob) results.largeBlob = { supported: !!provider.largeBlob };
        const credId = cred.id.slice().buffer;
        return {
          type: "public-key",
          id: b64u(cred.id),
          rawId: credId,
          authenticatorAttachment: "platform",
          response: {
            clientDataJSON: new TextEncoder().encode("{}").buffer,
            attestationObject: new Uint8Array([0xa0]).buffer,
            getTransports: () => ["internal", "hybrid"],
            getAuthenticatorData: () => authData(cred, true),
            getPublicKey: () => new Uint8Array([1, 2, 3]).buffer,
            getPublicKeyAlgorithm: () => -7,
          },
          getClientExtensionResults: () => results,
        };
      });
    },

    async get(options: CredentialRequestOptions): Promise<unknown> {
      const pk = options.publicKey!;
      const conditional = options.mediation === "conditional";
      return abortable(options.signal ?? undefined, async () => {
        const ext = (pk.extensions ?? {}) as Record<string, any>;
        extensionLog.push({ op: "get", mediation: options.mediation, ...ext });
        log.push(conditional ? "get:conditional" : "get");
        if (conditional) {
          await new Promise<void>((resolve, reject) => conditionalWaiters.push({ resolve, reject }));
        }
        if (cancelNext === "get") {
          cancelNext = null;
          throw dom("NotAllowedError");
        }
        checkRejects(ext);
        const allow = (pk.allowCredentials ?? []).map((d) => b64u(bytes(d.id)));
        const candidates = credentials.filter((c) => c.rpId === pk.rpId && (!allow.length || allow.includes(b64u(c.id))));
        // A chooser that returns undefined is a user who closes the sheet.
        const pickId = choose ? choose(candidates.map((c) => b64u(c.id))) : candidates[0] ? b64u(candidates[0].id) : undefined;
        const cred = candidates.find((c) => b64u(c.id) === pickId);
        if (!cred) throw dom("NotAllowedError", "no matching credential");
        const results: Record<string, unknown> = {};
        if (ext.prf?.eval?.first && cred.provider.prf) {
          results.prf = { enabled: true, results: { first: await prf(cred, ext.prf.eval.first) } };
        }
        if (ext.largeBlob) {
          if (!cred.provider.largeBlob) results.largeBlob = {};
          else if (ext.largeBlob.read) results.largeBlob = cred.blob ? { blob: cred.blob.slice().buffer } : {};
          else if (ext.largeBlob.write) {
            if (allow.length !== 1) throw dom("NotSupportedError", "largeBlob write needs one credential");
            cred.blob = bytes(ext.largeBlob.write).slice();
            results.largeBlob = { written: true };
          }
        }
        return {
          type: "public-key",
          id: b64u(cred.id),
          rawId: cred.id.slice().buffer,
          authenticatorAttachment: "platform",
          response: {
            clientDataJSON: new TextEncoder().encode("{}").buffer,
            authenticatorData: authData(cred, false),
            signature: new Uint8Array([9]).buffer,
            userHandle: cred.userId.slice().buffer,
          },
          getClientExtensionResults: () => results,
        };
      });
    },
  };

  const statics = {
    getClientCapabilities: async () => ({
      passkeyPlatformAuthenticator: true,
      conditionalGet: true,
      "extension:prf": true,
      "extension:largeBlob": false,
    }),
    signalUnknownCredential: async (arg: unknown) => {
      signalled.push({ kind: "unknown", arg });
    },
    signalAllAcceptedCredentials: async (arg: unknown) => {
      signalled.push({ kind: "all", arg });
    },
  };

  const prevNav = Object.getOwnPropertyDescriptor(globalThis.navigator ?? {}, "credentials");
  const prevPkc = Object.getOwnPropertyDescriptor(globalThis, "PublicKeyCredential");
  const prevSecure = Object.getOwnPropertyDescriptor(globalThis, "isSecureContext");
  if (!globalThis.navigator) Object.defineProperty(globalThis, "navigator", { value: {}, configurable: true });
  Object.defineProperty(globalThis.navigator, "credentials", { value: container, configurable: true });
  Object.defineProperty(globalThis, "PublicKeyCredential", { value: statics, configurable: true, writable: true });
  Object.defineProperty(globalThis, "isSecureContext", { value: true, configurable: true });

  return {
    credentials,
    log,
    signalled,
    extensionLog,
    use(next: ProviderBehaviour) {
      provider = next;
    },
    cancelNext(kind: "create" | "get") {
      cancelNext = kind;
    },
    choose(fn: ((ids: string[]) => string | undefined) | null) {
      choose = fn;
    },
    /** Let the pending conditional (autofill) request pick a passkey. */
    releaseConditional() {
      conditionalWaiters.shift()?.resolve();
    },
    get conditionalPending() {
      return conditionalWaiters.length;
    },
    uninstall() {
      if (prevNav) Object.defineProperty(globalThis.navigator, "credentials", prevNav);
      else Reflect.deleteProperty(globalThis.navigator, "credentials");
      if (prevPkc) Object.defineProperty(globalThis, "PublicKeyCredential", prevPkc);
      else Reflect.deleteProperty(globalThis, "PublicKeyCredential");
      if (prevSecure) Object.defineProperty(globalThis, "isSecureContext", prevSecure);
      else Reflect.deleteProperty(globalThis, "isSecureContext");
    },
  };
}

export const PROVIDERS = {
  gpm: { name: "gpm", prf: true, prfAtCreate: true, synced: true, aaguid: "ea9b8d66-4d01-1d21-3ce4-b6b48cb575d4" },
  icloud: { name: "icloud", prf: true, prfAtCreate: true, synced: true, largeBlob: true, aaguid: "fbfc3007-154e-4ecc-8c0b-6e020557d7bd" },
  yubikey: { name: "yubikey", prf: true, prfAtCreate: false, synced: false, largeBlob: true },
  windowsHelloNoPrf: { name: "hello", prf: false, synced: false, aaguid: "08987058-cadc-4b81-b6e1-30de50dcbe96" },
  oldManager: { name: "old-manager", prf: true, prfAtCreate: false, synced: true, rejects: ["largeBlob"] },
} satisfies Record<string, ProviderBehaviour>;
