/**
 * Every failure the package raises is a {@link PasskeyError} with a stable
 * `code`. Branch on the code, not on the message: browsers word WebAuthn
 * errors differently and change them between releases.
 */
export type PasskeyErrorCode =
  /** The user closed the passkey sheet, or it timed out. Not worth reporting. */
  | "cancelled"
  /** An AbortSignal you passed fired, or a newer ceremony replaced this one. */
  | "aborted"
  /** This browser has no WebAuthn, or the page is not a secure context. */
  | "unsupported"
  /** The RP ID does not match this origin (or `127.0.0.1` was used instead of `localhost`). */
  | "rp-id"
  /** `create()` matched `excludeCredentials`: this provider already holds a passkey for the vault. */
  | "already-registered"
  /**
   * The authenticator cannot derive a PRF secret, so it cannot protect a
   * secret. Offer another passkey provider or a passphrase instead.
   */
  | "prf-unsupported"
  /** No vault is saved on this device (and the passkey carried no portable copy). */
  | "no-vault"
  /** A vault already exists; pass `{ replace: true }` to overwrite it. */
  | "vault-exists"
  /** The chosen passkey is not one of this vault's passkeys. */
  | "wrong-passkey"
  /** The ciphertext did not open with this passkey (corrupt, or a different vault). */
  | "decrypt-failed"
  /** Another tab changed the vault while this operation ran. Reload and retry. */
  | "conflict"
  /** Removing this passkey would leave the vault with none. */
  | "last-passkey"
  | "unknown";

export class PasskeyError extends Error {
  readonly code: PasskeyErrorCode;
  constructor(code: PasskeyErrorCode, message: string, options?: { cause?: unknown }) {
    super(message, options);
    this.name = "PasskeyError";
    this.code = code;
  }
}

export function isPasskeyError(error: unknown, code?: PasskeyErrorCode): error is PasskeyError {
  return error instanceof PasskeyError && (code === undefined || error.code === code);
}

/** The user dismissed the sheet or the operation was aborted: stay quiet. */
export function isPasskeyCancel(error: unknown): boolean {
  if (error instanceof PasskeyError) return error.code === "cancelled" || error.code === "aborted";
  return domName(error) === "NotAllowedError" || domName(error) === "AbortError";
}

function domName(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "name" in error ? String((error as { name: unknown }).name) : undefined;
}

/**
 * Map a WebAuthn `DOMException` to a {@link PasskeyError}. `phase` picks the
 * right reading of the ambiguous names: `InvalidStateError` means "already
 * registered" only for `create()`.
 */
export function toPasskeyError(error: unknown, phase: "create" | "get" | "crypto" = "get"): PasskeyError {
  if (error instanceof PasskeyError) return error;
  const name = domName(error);
  const detail = error instanceof Error && error.message ? error.message : String(error);
  switch (name) {
    case "NotAllowedError":
      return new PasskeyError("cancelled", "The passkey request was cancelled or timed out.", { cause: error });
    case "AbortError":
      return new PasskeyError("aborted", "The passkey request was aborted.", { cause: error });
    case "InvalidStateError":
      return phase === "create"
        ? new PasskeyError("already-registered", "This passkey provider already has a passkey for this wallet.", { cause: error })
        : new PasskeyError("unknown", detail, { cause: error });
    case "SecurityError":
      return new PasskeyError("rp-id", `The passkey RP ID does not match this site: ${detail}`, { cause: error });
    case "NotSupportedError":
      return new PasskeyError("unsupported", `This browser or authenticator does not support the request: ${detail}`, { cause: error });
    default:
      return new PasskeyError("unknown", detail, { cause: error });
  }
}

export function abortError(signal?: AbortSignal): PasskeyError {
  const reason = signal?.reason;
  if (reason instanceof PasskeyError) return reason;
  return new PasskeyError("aborted", "The passkey request was aborted.", { cause: reason });
}

export function throwIfAborted(signal?: AbortSignal): void {
  if (signal?.aborted) throw abortError(signal);
}

/**
 * Settle with `work`, or reject when `signal` aborts. `committed()` means the
 * change is already stored: an abort must not report a cancel that did not
 * happen, including an abort that lands in the gap before the flag is read.
 * While `committing()` is true the abort waits for `work`, so a commit that
 * finishes successfully still resolves.
 */
export function raceAbort<T>(
  signal: AbortSignal | undefined,
  work: Promise<T>,
  committed?: () => boolean,
  committing?: () => boolean,
): Promise<T> {
  if (!signal) return work;
  if (signal.aborted && !committed?.() && !committing?.()) {
    work.catch(() => {});
    return Promise.reject(abortError(signal));
  }
  return new Promise<T>((resolve, reject) => {
    let aborting = signal.aborted;
    const finishAbort = () => {
      if (committed?.() || committing?.()) return;
      reject(abortError(signal));
    };
    const onAbort = () => {
      aborting = true;
      finishAbort();
    };
    if (!signal.aborted) signal.addEventListener("abort", onAbort, { once: true });
    work.then(
      (value) => {
        signal.removeEventListener("abort", onAbort);
        if (aborting && !committed?.()) reject(abortError(signal));
        else resolve(value);
      },
      (error) => {
        signal.removeEventListener("abort", onAbort);
        reject(error);
      },
    );
  });
}
