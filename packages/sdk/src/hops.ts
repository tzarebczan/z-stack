/**
 * Composable, fail-closed network hops for browser and Node/Electron consumers.
 *
 * Hops nest cleanly:
 * - `directHop()`                        -> plain fetch (loopback / explicit opt-in)
 * - `nymHop({ mixFetch })`               -> Nym mixnet hop (browser IP protection)
 * - `ohttpHop({ inner: directHop() })`   -> standard OHTTP
 * - `ohttpHop({ inner: nymHop(...) })`   -> Nym + OHTTP (double-blinded!)
 *
 * All HPKE / OHTTP cryptography stays in Rust/WASM (`OhttpSealer` supplied by Rust).
 */

import { transportFetchOptions } from "./lwd";

export type HopRole = "read" | "submit" | "pir" | "external";

export type HopKind = "direct" | "nym" | "tor" | "ohttp";

export interface HopRequest {
  method: "GET" | "POST";
  /** Full URL for direct/proxy/RFC-9458 hops, or inner route path for gateway hops. */
  target: string;
  headers?: Record<string, string>;
  body?: Uint8Array;
  role: HopRole;
  maxResponseBytes?: number;
  timeoutMs?: number;
  /** Only honored for non-submit roles; `submit` NEVER auto-retries. */
  idempotent?: boolean;
}

export interface HopResponse {
  status: number;
  headers: Record<string, string>;
  body: Uint8Array;
}

export class HopError extends Error {
  readonly code:
    | "closed"
    | "unavailable"
    | "fail_closed"
    | "timeout"
    | "size_limit"
    | "invalid_request"
    | "protocol"
    | "ohttp";

  constructor(code: HopError["code"], message: string) {
    super(message);
    this.name = "HopError";
    this.code = code;
  }

  get retryable(): boolean {
    return this.code === "unavailable" || this.code === "timeout";
  }
}

export interface NetworkHop {
  readonly kind: HopKind;
  readonly label: string;
  exchange(request: HopRequest, signal: AbortSignal): Promise<HopResponse>;
  close(): Promise<void> | void;
}

export interface RetryPolicy {
  maxAttempts: number;
  baseDelayMs: number;
  maxDelayMs: number;
}

export const DEFAULT_HOP_RETRY: RetryPolicy = Object.freeze({
  maxAttempts: 3,
  baseDelayMs: 250,
  maxDelayMs: 4_000,
});

export const NO_HOP_RETRY: RetryPolicy = Object.freeze({
  maxAttempts: 1,
  baseDelayMs: 0,
  maxDelayMs: 0,
});

const DEFAULT_MAX_RESPONSE_BYTES = 16 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS = 30_000;

async function readBoundedBody(
  response: Response,
  limit: number,
  signal: AbortSignal,
): Promise<Uint8Array> {
  const declared = response.headers.get("content-length");
  if (declared !== null && (!/^\d+$/.test(declared.trim()) || Number(declared.trim()) > limit)) {
    void response.body?.cancel().catch(() => {});
    throw new HopError("size_limit", "Response exceeds configured size limit.");
  }
  if (!response.body) {
    const buf = new Uint8Array(await response.arrayBuffer());
    if (buf.byteLength > limit) {
      throw new HopError("size_limit", "Response exceeds configured size limit.");
    }
    return buf;
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      signal.throwIfAborted();
      const { done, value } = await reader.read();
      if (done) break;
      if (value && value.byteLength > 0) {
        total += value.byteLength;
        if (total > limit) {
          void reader.cancel().catch(() => {});
          throw new HopError("size_limit", "Response exceeds configured size limit.");
        }
        chunks.push(value);
      }
    }
  } finally {
    reader.releaseLock();
  }
  if (chunks.length === 0) return new Uint8Array(0);
  if (chunks.length === 1) return chunks[0]!;
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return out;
}

function responseHeaders(response: Response): Record<string, string> {
  const out: Record<string, string> = {};
  response.headers.forEach((value, key) => {
    out[key.toLowerCase()] = value;
  });
  return out;
}

function combinedSignal(signal: AbortSignal, timeoutMs: number): AbortSignal {
  const clamped = Math.max(500, Math.min(120_000, Math.trunc(timeoutMs)));
  return AbortSignal.any([signal, AbortSignal.timeout(clamped)]);
}

function stripAmbientHeaders(headers?: Record<string, string>): Record<string, string> {
  const out: Record<string, string> = {};
  if (!headers) return out;
  for (const [k, v] of Object.entries(headers)) {
    const lower = k.toLowerCase();
    if (
      lower === "cookie" ||
      lower === "origin" ||
      lower === "referer" ||
      lower === "x-forwarded-for" ||
      lower === "cf-connecting-ip"
    ) {
      continue;
    }
    out[lower] = v;
  }
  return out;
}

function validateEndpointUrl(raw: string, label: string): string {
  const trimmed = raw.trim();
  if (!trimmed || /[\u0000-\u001f\u007f]/.test(trimmed)) {
    throw new HopError("invalid_request", `${label} URL is invalid.`);
  }
  let parsed: URL;
  try {
    parsed = new URL(trimmed);
  } catch {
    throw new HopError("invalid_request", `${label} URL must be a valid absolute URL.`);
  }
  if (parsed.username || parsed.password) {
    throw new HopError("invalid_request", `${label} URL must not include embedded credentials.`);
  }
  const host = parsed.hostname.toLowerCase();
  const isLoopback = host === "127.0.0.1" || host === "localhost" || host === "[::1]" || host === "::1";
  if (parsed.protocol === "https:") {
    return trimmed;
  }
  if (parsed.protocol === "http:" && isLoopback) {
    return trimmed;
  }
  throw new HopError(
    "invalid_request",
    `${label} URL must use https:// (or http:// on exact loopback only).`,
  );
}

/** Direct HTTPS / loopback HTTP hop. Never sends cookies, referer, or follows redirects. */
export function directHop(opts: { fetch?: typeof fetch; label?: string } = {}): NetworkHop {
  const fetchImpl = opts.fetch ?? globalThis.fetch.bind(globalThis);
  let closed = false;
  return {
    kind: "direct",
    label: opts.label ?? "Direct",
    async exchange(request, signal) {
      if (closed) throw new HopError("closed", "Direct hop is closed.");
      signal.throwIfAborted();
      const target = validateEndpointUrl(request.target, "Direct target");
      const wait = combinedSignal(signal, request.timeoutMs ?? DEFAULT_TIMEOUT_MS);
      let response: Response;
      try {
        response = await fetchImpl(target, {
          ...transportFetchOptions,
          method: request.method,
          headers: stripAmbientHeaders(request.headers),
          body: request.body ? new Uint8Array(request.body) : undefined,
          signal: wait,
        });
      } catch (err) {
        if (signal.aborted) throw err;
        if (err instanceof DOMException && err.name === "TimeoutError") {
          throw new HopError("timeout", "Request timed out.");
        }
        throw new HopError("unavailable", "Network request failed.");
      }
      if (closed) throw new HopError("closed", "Direct hop closed during request.");
      const body = await readBoundedBody(
        response,
        request.maxResponseBytes ?? DEFAULT_MAX_RESPONSE_BYTES,
        wait,
      );
      return {
        status: response.status,
        headers: responseHeaders(response),
        body,
      };
    },
    close() {
      closed = true;
    },
  };
}

export type MixFetchFn = (url: string, init: RequestInit) => Promise<Response>;

/**
 * Browser Nym mixnet hop (`@nymproject/mix-fetch` or caller-provided mixnet transport).
 *
 * Strictly fail-closed across all roles (`read`, `submit`, `pir`, `external`): if the
 * mixnet cannot start or fails during dispatch, the request is never silently sent direct.
 */
export function nymHop(opts: {
  connect: (signal: AbortSignal) => Promise<MixFetchFn>;
  onDegradedChange?: (degraded: boolean) => void;
  label?: string;
}): NetworkHop {
  let closed = false;
  let degraded = false;
  const setDegraded = (next: boolean) => {
    if (degraded === next) return;
    degraded = next;
    opts.onDegradedChange?.(next);
  };
  return {
    kind: "nym",
    label: opts.label ?? "Nym Mixnet",
    async exchange(request, signal) {
      if (closed) throw new HopError("closed", "Nym hop is closed.");
      signal.throwIfAborted();
      const target = validateEndpointUrl(request.target, "Mixnet target");
      const wait = combinedSignal(signal, request.timeoutMs ?? DEFAULT_TIMEOUT_MS);
      let mix: MixFetchFn;
      try {
        mix = await opts.connect(wait);
      } catch (err) {
        if (signal.aborted) throw err;
        setDegraded(true);
        throw new HopError("fail_closed", "Mixnet is unavailable; request was not sent.");
      }
      if (closed) throw new HopError("closed", "Nym hop closed before dispatch.");
      signal.throwIfAborted();
      let response: Response;
      try {
        response = await mix(target, {
          ...transportFetchOptions,
          method: request.method,
          headers: stripAmbientHeaders(request.headers),
          body: request.body ? new Uint8Array(request.body) : undefined,
          signal: wait,
        });
      } catch (err) {
        if (signal.aborted) throw err;
        setDegraded(true);
        if (err instanceof DOMException && err.name === "TimeoutError") {
          throw new HopError("timeout", "Mixnet request timed out.");
        }
        throw new HopError("fail_closed", "Mixnet request failed after dispatch.");
      }
      setDegraded(false);
      if (closed) throw new HopError("closed", "Nym hop closed during response.");
      const body = await readBoundedBody(
        response,
        request.maxResponseBytes ?? DEFAULT_MAX_RESPONSE_BYTES,
        wait,
      );
      return {
        status: response.status,
        headers: responseHeaders(response),
        body,
      };
    },
    async close() {
      closed = true;
    },
  };
}

/** Rust/WASM-owned OHTTP encapsulation context (never implemented in TypeScript). */
export interface OhttpExchangeContext {
  readonly requestBody: Uint8Array;
  readonly responseLimit: number;
  finish(encryptedReply: Uint8Array): HopResponse;
  cancel(): void;
}

export interface OhttpSealer {
  seal(request: HopRequest, signal: AbortSignal): Promise<OhttpExchangeContext>;
  close?(): Promise<void> | void;
}

/**
 * Composable OHTTP hop wrapping ANY inner `NetworkHop` (`directHop` or `nymHop`).
 *
 * HPKE/BHTTP cryptography stays inside `sealer` (Rust/WASM); this layer handles
 * bounded relay dispatch, fail-closed semantics, and inner-hop composition.
 */
export function ohttpHop(opts: {
  relayUrl: string;
  inner: NetworkHop;
  sealer: OhttpSealer;
  label?: string;
}): NetworkHop {
  const relayUrl = validateEndpointUrl(opts.relayUrl, "OHTTP relay");
  let closed = false;
  return {
    kind: "ohttp",
    label: opts.label ?? `OHTTP via ${opts.inner.label}`,
    async exchange(request, signal) {
      if (closed) throw new HopError("closed", "OHTTP hop is closed.");
      signal.throwIfAborted();
      const ctx = await opts.sealer.seal(request, signal);
      try {
        if (closed) throw new HopError("closed", "OHTTP hop closed before dispatch.");
        const outer = await opts.inner.exchange(
          {
            method: "POST",
            target: relayUrl,
            headers: {
              "content-type": "message/ohttp-req",
              accept: "message/ohttp-res",
            },
            body: ctx.requestBody,
            role: request.role,
            maxResponseBytes: ctx.responseLimit,
            timeoutMs: request.timeoutMs,
            // Never replay an already-sealed HPKE envelope on the inner hop;
            // outer idempotent retries must re-seal with fresh HPKE randomness.
            idempotent: false,
          },
          signal,
        );
        if (outer.status !== 200) {
          throw new HopError("unavailable", `OHTTP relay returned HTTP ${outer.status}.`);
        }
        return ctx.finish(outer.body);
      } catch (err) {
        ctx.cancel();
        throw err;
      }
    },
    async close() {
      closed = true;
      await opts.sealer.close?.();
      await opts.inner.close();
    },
  };
}

export interface SplitHopRouter {
  readonly epoch: number;
  hopFor(role: HopRole): NetworkHop;
  exchange(request: HopRequest, signal: AbortSignal): Promise<HopResponse>;
  closeAll(): Promise<void>;
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  if (ms <= 0) return Promise.resolve();
  return new Promise((resolve, reject) => {
    if (signal.aborted) {
      reject(signal.reason ?? new DOMException("Aborted", "AbortError"));
      return;
    }
    const timer = setTimeout(() => {
      signal.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(timer);
      reject(signal.reason ?? new DOMException("Aborted", "AbortError"));
    };
    signal.addEventListener("abort", onAbort, { once: true });
  });
}

/**
 * Routes `read`, `submit`, `pir`, and `external` requests to composable hops
 * with strict epoch fencing (closing cancels all stale callers) and bounded
 * retry on idempotent reads ONLY (never on `submit`).
 */
export function createSplitHopRouter(opts: {
  defaultHop: NetworkHop;
  readHop?: NetworkHop;
  submitHop?: NetworkHop;
  pirHop?: NetworkHop;
  externalHop?: NetworkHop;
  retry?: RetryPolicy;
}): SplitHopRouter {
  const hops: Record<HopRole, NetworkHop> = {
    read: opts.readHop ?? opts.defaultHop,
    submit: opts.submitHop ?? opts.defaultHop,
    pir: opts.pirHop ?? opts.defaultHop,
    external: opts.externalHop ?? opts.defaultHop,
  };
  const retry = opts.retry ?? DEFAULT_HOP_RETRY;
  let epoch = 1;
  let closed = false;

  return {
    get epoch() {
      return epoch;
    },
    hopFor(role) {
      return hops[role];
    },
    async exchange(request, signal) {
      if (closed) throw new HopError("closed", "Hop router is closed.");
      const startEpoch = epoch;
      const hop = hops[request.role];
      const canRetry =
        request.role !== "submit" &&
        (request.idempotent ?? (request.role === "read" || request.role === "pir"));
      const attempts = canRetry ? Math.max(1, retry.maxAttempts) : 1;

      let lastErr: unknown = new HopError("unavailable", "No hop attempt executed.");
      for (let attempt = 0; attempt < attempts; attempt++) {
        if (closed || epoch !== startEpoch) {
          throw new HopError("closed", "Hop router closed.");
        }
        signal.throwIfAborted();
        try {
          const resp = await hop.exchange(request, signal);
          if (closed || epoch !== startEpoch) {
            throw new HopError("closed", "Hop router closed during response.");
          }
          if (
            canRetry &&
            (resp.status === 502 || resp.status === 503 || resp.status === 504) &&
            attempt + 1 < attempts
          ) {
            const jitter = 0.75 + Math.random() * 0.5;
            const delay = Math.min(
              retry.maxDelayMs,
              Math.round(retry.baseDelayMs * Math.pow(2, attempt) * jitter),
            );
            await sleep(delay, signal);
            continue;
          }
          return resp;
        } catch (err) {
          if (signal.aborted) throw err;
          const retryable = err instanceof HopError && err.retryable;
          if (!canRetry || !retryable || attempt + 1 >= attempts) {
            throw err;
          }
          lastErr = err;
          const jitter = 0.75 + Math.random() * 0.5;
          const delay = Math.min(
            retry.maxDelayMs,
            Math.round(retry.baseDelayMs * Math.pow(2, attempt) * jitter),
          );
          await sleep(delay, signal);
        }
      }
      throw lastErr;
    },
    async closeAll() {
      closed = true;
      epoch += 1;
      const unique = new Set(Object.values(hops));
      await Promise.all([...unique].map((h) => Promise.resolve(h.close())));
    },
  };
}
