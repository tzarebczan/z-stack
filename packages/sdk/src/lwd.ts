/**
 * Compact-block transports for the WASM scanner.
 * Crypto stays in z-wasm; this only moves CompactBlock bytes.
 */

import type { Network } from "./runtime";

/** Keep ambient browser identity and redirects out of chain/bridge requests. */
export const transportFetchOptions = {
  credentials: "omit",
  referrerPolicy: "no-referrer",
  redirect: "error",
} as const satisfies RequestInit;

export type TransparentUtxo = {
  txid: string;
  index: number;
  script: string;
  valueZat: number;
  height: number;
  address: string;
};

export type BlockTransport = {
  kind: string;
  label: string;
  tip: (signal?: AbortSignal) => Promise<number>;
  /** Length-delimited compact blocks (u32 BE + protobuf), start..=end inclusive. */
  blocks: (start: number, end: number, signal?: AbortSignal) => Promise<Uint8Array>;
  /** Wallet-independent all-pool ranges; negotiate protocolVersion >= 0.5 first. */
  transparentBlocks?: (start: number, end: number, signal?: AbortSignal) => Promise<Uint8Array>;
  /** Public range bundle: compact blocks and every shielded raw transaction. */
  sharedMemos?: (start: number, end: number, signal?: AbortSignal) => Promise<string | null>;
  /**
   * Broadcast a raw transaction hex. Loopback proxy posts `/lwd/sendraw`.
   * The node's explicit refusal throws a {@link BroadcastRejection}; any
   * other error is an unknown outcome.
   */
  submit?: (rawHex: string) => Promise<string>;
  /** Loopback-only. Leaks t-addrs to local Zaino; never call against public LWD. */
  utxos?: (addresses: string[], startHeight?: number, signal?: AbortSignal) => Promise<TransparentUtxo[]>;
  /** Loopback-only regtest mine via Zebra `generate`. */
  mine?: (blocks: number) => Promise<{ mined: number }>;
  /** Loopback-only raw mempool txs (hex). WASM trial-decrypts; no t-addrs sent. */
  mempool?: () => Promise<Array<{ txid?: string; hex: string }>>;
  /**
   * Full transaction hex for memo enhancement. `txid` is the 64-character
   * explorer/display-order hex ID, as returned by wallet history. Built-in
   * transports handle wire byte order; callers must not reverse it.
   * Fetches `GET /lwd/tx` or gRPC-Web GetTransaction.
   */
  tx?: (txid: string, signal?: AbortSignal) => Promise<string>;
  /** Birthday-1 `GetTreeState` (frontiers for spend witnesses). */
  treeState?: (height: number, signal?: AbortSignal) => Promise<{
    height: number;
    hash: string;
    saplingTree?: string;
    orchardTree?: string;
    ironwoodTree?: string;
    network?: string;
    time?: number;
  }>;
  /** CompactTxStreamer `GetLightdInfo` (chain name / vendor). */
  info?: () => Promise<{ chain: string; blockHeight?: number; vendor?: string; version?: string; consensusBranchId?: string; protocolVersion?: string; transparentCompact?: boolean }>;
  /**
   * Complete shard roots (`GetSubtreeRoots`) from shard `startIndex`, at most
   * `maxEntries` of them (0 = all). A pipe older than `maxEntries` returns
   * every root from `startIndex`.
   */
  subtreeRoots?: (
    protocol: "sapling" | "orchard" | "ironwood",
    startIndex?: number,
    signal?: AbortSignal,
    maxEntries?: number,
  ) => Promise<Array<{ completingHeight: number; rootHash: string }>>;
  /**
   * Compact-block stream for start..=end. Pages one HTTP `/lwd/blocks` at
   * the 8,000-block HTTP cap and applies `page` blocks as the body arrives.
   * WASM `runSync` does **not** use this — it prefetches overlapping
   * `blocks()` GETs so Network shows more than one in-flight request.
   * Callers that pass a span larger than the 8,000-block HTTP cap are paged;
   * never issue a 100k GET (Windows aborts the pipe).
   */
  blockStream?: (
    start: number,
    end: number,
    page: number,
    onPage: (blob: Uint8Array, blocks: number) => Promise<void>,
  ) => Promise<void>;
};

export function isLoopbackUrl(url: string): boolean {
  try {
    const u = new URL(url.includes("://") ? url : `http://${url}`);
    const host = u.hostname.replace(/^\[|\]$/g, "");
    return host === "127.0.0.1" || host === "localhost" || host === "::1";
  } catch {
    return false;
  }
}

/** Shared public LWD (batch-capped). A user-run Zaino on a public IP is not this. */
export function isPublicLwdUrl(url: string): boolean {
  try {
    const u = new URL(url.includes("://") ? url : `http://${url}`);
    const host = u.hostname.toLowerCase();
    return host === "zec.rocks" || host.endsWith(".zec.rocks");
  } catch {
    return false;
  }
}

/** Local / dedicated Zaino uses 4000-block batches. zec.rocks uses 1000. */
export function usesFastSync(url?: string): boolean {
  const s = (url ?? "").trim();
  if (!s) return true;
  return !isPublicLwdUrl(s);
}

/** Product t-scan: only a user-run Zaino (loopback). Public LWD is shield-only. */
export function allowsTransparentQuery(server?: string): boolean {
  const s = (server ?? "").trim();
  if (!s) return true;
  const lower = s.toLowerCase();
  if (lower === "local" || lower === "local-zaino" || lower === "local-regtest") {
    return true;
  }
  return isLoopbackUrl(s);
}

/**
 * Browser can talk to this URL without `z-wallet serve`.
 * Native gRPC (`:8137`) is not this — that still needs the loopback proxy.
 */
export function looksLikeGrpcWeb(url: string): boolean {
  try {
    const u = new URL(url.includes("://") ? url : `http://${url}`);
    const port = u.port || (u.protocol === "https:" ? "443" : "80");
    if (port === "1239") return false;
    if (port === "1234" || port === "1238") return true;
    if (u.pathname.toLowerCase().includes("zaino")) return true;
    const host = u.hostname.toLowerCase();
    return host === "zec.rocks" || host.endsWith(".zec.rocks");
  } catch {
    return false;
  }
}

/** One `GET /lwd/blocks` on `z-wallet pipe` (matches Rust `PIPE_MAX_BLOCKS`). */
export const PIPE_HTTP_MAX = 8_000;

/** `z-wallet pipe` — native gRPC fan-out, one HTTP stream to the tab. */
export function looksLikeLwdPipe(url: string): boolean {
  try {
    const u = new URL(url.includes("://") ? url : `http://${url}`);
    const port = u.port || (u.protocol === "https:" ? "443" : "80");
    return port === "1239";
  } catch {
    return false;
  }
}

/** Parse u32-BE length-delimited compact blocks from a chunked HTTP body.
 *  Double-buffers: the next HTTP chunks are read while `onPage` (WASM apply) runs.
 */
export async function consumeDelimitedStream(
  body: ReadableStream<Uint8Array>,
  pageBlocks: number,
  onPage: (blob: Uint8Array, blocks: number) => Promise<void>,
): Promise<void> {
  const pageSize = Math.max(1, Math.floor(pageBlocks) || 1);
  const reader = body.getReader();
  let buf = new Uint8Array(0);
  const parts: Uint8Array[] = [];
  let count = 0;
  let pending: Promise<void> = Promise.resolve();

  const flush = async () => {
    if (count === 0) return;
    const blob = concatBytes(parts);
    parts.length = 0;
    const n = count;
    count = 0;
    await pending;
    pending = onPage(blob, n);
  };

  const push = async (framed: Uint8Array) => {
    parts.push(framed);
    count += 1;
    if (count >= pageSize) await flush();
  };

  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (!value || value.byteLength === 0) continue;
      const next = new Uint8Array(buf.byteLength + value.byteLength);
      next.set(buf, 0);
      next.set(value, buf.byteLength);
      buf = next;
      let i = 0;
      while (i + 4 <= buf.byteLength) {
        const n = new DataView(buf.buffer, buf.byteOffset + i, 4).getUint32(0, false);
        if (i + 4 + n > buf.byteLength) break;
                await push(buf.slice(i, i + 4 + n));
        i += 4 + n;
      }
      buf = buf.subarray(i);
    }
  } finally {
    reader.releaseLock();
  }
  if (buf.byteLength !== 0) {
    throw new Error(`truncated compact-block stream (${buf.byteLength} leftover bytes)`);
  }
  await flush();
  await pending;
}

const STREAMER = "/cash.z.wallet.sdk.rpc.CompactTxStreamer";

/** Map serve 401s to an actionable message (Zaino itself has no bridge token). */
export function explainBridgeAuthError(status: number, raw?: string): string | undefined {
  const msg = (raw ?? "").toLowerCase();
  if (status !== 401 && !msg.includes("bridge token")) return undefined;
  if (msg.includes("invalid")) {
    return "bridge token invalid — z-wallet serve prints a new token each start; paste it into Bridge token (not a Zaino credential).";
  }
  return "bridge token required — the Proxy field is z-wallet serve (127.0.0.1:8787), not Zaino. For gRPC-Web on :1238 / :1234, set Block transport to gRPC-Web (no token). Otherwise paste the token from `z-wallet serve` stdout (Authorization: Bearer).";
}

/** Strip `?token=` from a loopback URL and return `{ base, token }`. */
export function splitProxyAuth(
  url: string,
  explicitToken?: string,
): { base: string; token?: string } {
  const raw = url.trim();
  let token = explicitToken?.trim() || undefined;
  try {
    const u = new URL(raw.includes("://") ? raw : `http://${raw}`);
    const q = u.searchParams.get("token")?.trim();
    if (!token && q) token = q;
    u.search = "";
    u.hash = "";
    return { base: u.toString().replace(/\/$/, ""), token: token || undefined };
  } catch {
    return { base: raw.replace(/\/$/, ""), token: token || undefined };
  }
}

function encodeVarint(n: number): number[] {
  const out: number[] = [];
  let x = n >>> 0;
  while (x > 0x7f) {
    out.push((x & 0x7f) | 0x80);
    x >>>= 7;
  }
  out.push(x);
  return out;
}

function encodeBlockId(height: number): Uint8Array {
  return new Uint8Array([0x08, ...encodeVarint(height)]);
}

function encodeBlockRange(start: number, end: number, allPools = false): Uint8Array {
  const a = encodeBlockId(start);
  const b = encodeBlockId(end);
  return new Uint8Array([0x0a, a.length, ...a, 0x12, b.length, ...b, ...(allPools ? [0x1a, 4, 1, 2, 3, 4] : [])]);
}

/** `GetSubtreeRootsArg`: start_index=1, shielded_protocol=2, max_entries=3. */
function encodeGetSubtreeRoots(protocol: number, startIndex: number, maxEntries = 0): Uint8Array {
  const limit = Math.max(0, maxEntries | 0);
  return new Uint8Array([
    0x08,
    ...encodeVarint(Math.max(0, startIndex | 0)),
    0x10,
    ...encodeVarint(protocol),
    ...(limit > 0 ? [0x18, ...encodeVarint(limit)] : []),
  ]);
}

function protoBytes(buf: Uint8Array, field: number): Uint8Array | undefined {
  let i = 0;
  while (i < buf.length) {
    const [key, next] = readVarint(buf, i);
    i = next;
    const f = key >> 3;
    const wire = key & 7;
    if (wire === 0) {
      const [, j] = readVarint(buf, i);
      i = j;
    } else if (wire === 2) {
      const [len, j] = readVarint(buf, i);
      const sl = buf.subarray(j, j + len);
      i = j + len;
      if (f === field) return sl;
    } else if (wire === 1) {
      i += 8;
    } else if (wire === 5) {
      i += 4;
    } else {
      break;
    }
  }
  return undefined;
}

function toHex(bytes: Uint8Array): string {
  let s = "";
  for (let i = 0; i < bytes.length; i++) {
    s += bytes[i]!.toString(16).padStart(2, "0");
  }
  return s;
}

function decodeSubtreeRoot(buf: Uint8Array): { completingHeight: number; rootHash: string } {
  // Field 1 is reserved. Match zakura-client-backend's SubtreeRoot schema:
  // root_hash=2, completing_block_hash=3, completing_block_height=4.
  const fields = protoFieldsOf(buf, true);
  const hash = fields.find(f => f.field === 2)?.value;
  const height = fields.find(f => f.field === 4)?.value;
  // Never filter a malformed streamed root: that shifts every later shard's
  // index and could install a root at the wrong position.
  if (!(hash instanceof Uint8Array) || hash.length !== 32 || typeof height !== "number"
      || !Number.isSafeInteger(height) || height > 0xffff_ffff) {
    throw new Error("invalid subtree root response");
  }
  return { completingHeight: height, rootHash: toHex(hash) };
}

function shieldedProtocol(protocol: "sapling" | "orchard" | "ironwood"): number {
  if (protocol === "orchard") return 1;
  if (protocol === "ironwood") return 2;
  return 0;
}

function frameGrpc(payload: Uint8Array): Uint8Array {
  const out = new Uint8Array(5 + payload.length);
  out[0] = 0;
  new DataView(out.buffer).setUint32(1, payload.length, false);
  out.set(payload, 5);
  return out;
}

function grpcStatusError(status: string | null | undefined, rawMessage: string | null | undefined): void {
  if (!status || status.trim() === "0") return;
  let message = rawMessage ?? "";
  try { message = decodeURIComponent(message); } catch { /* keep raw */ }
  throw new Error(`grpc-web status ${status.trim()}${message ? `: ${message}` : ""}`);
}

/** Response messages, plus the call's grpc-status once a trailer (or header) carried one. */
type GrpcMessages = Uint8Array[] & { grpcStatus?: string };

function parseGrpcWeb(buf: Uint8Array): GrpcMessages {
  const messages: GrpcMessages = [];
  let i = 0;
  while (i + 5 <= buf.length) {
    const flag = buf[i];
    const len = new DataView(buf.buffer, buf.byteOffset + i + 1, 4).getUint32(0, false);
    i += 5;
    if (i + len > buf.length) break;
    const slice = buf.subarray(i, i + len);
    i += len;
    if (flag & 0x80) {
      const trailers = new TextDecoder().decode(slice);
      const status = /grpc-status:\s*(\d+)/i.exec(trailers)?.[1];
      grpcStatusError(status, /grpc-message:\s*([^\r\n]*)/i.exec(trailers)?.[1]);
      if (status) messages.grpcStatus = status;
      continue;
    }
    messages.push(slice);
  }
  return messages;
}

function toDelimited(parts: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const p of parts) total += 4 + p.length;
  const out = new Uint8Array(total);
  let o = 0;
  for (const p of parts) {
    new DataView(out.buffer, o, 4).setUint32(0, p.length, false);
    o += 4;
    out.set(p, o);
    o += p.length;
  }
  return out;
}

function concatBytes(chunks: Uint8Array[]): Uint8Array {
  let n = 0;
  for (const c of chunks) n += c.length;
  const out = new Uint8Array(n);
  let o = 0;
  for (const c of chunks) {
    out.set(c, o);
    o += c.length;
  }
  return out;
}

const BLOCK_FETCH_RETRY_MS = [100, 250] as const;
/** A single 1k gRPC-Web range must fail before the outer 90s reconnect grace. */
const GRPC_WEB_BLOCK_TIMEOUT_MS = 30_000;

export class TransientLightServerError extends Error {}

/** The node refused a broadcast: its reply code and reason. */
export class BroadcastRejection extends Error {
  readonly code: number;
  readonly reason: string;
  constructor(code: number, reason: string) {
    super(`SendTransaction rejected (${code}): ${reason || "no reason given"}`);
    this.name = "BroadcastRejection";
    this.code = code;
    this.reason = reason;
  }
}

export function isBroadcastRejection(error: unknown): error is BroadcastRejection {
  return error instanceof BroadcastRejection
    || (error instanceof Error && error.name === "BroadcastRejection" && typeof (error as BroadcastRejection).reason === "string");
}

/**
 * The node already has *this* transaction. Matched from the start of the
 * reason so "nullifier already known" and "conflicts with a transaction
 * already in the mempool" stay rejections.
 *
 * Zebra wraps an already-mined transaction as
 * "any transaction with the same effects …: transaction was committed to the best chain".
 * That whole reason is a duplicate. A different suffix is still a refusal.
 * Zebra also suppresses duplicate submissions already in its mempool or
 * download queue. Queued means delivery is underway, not that validation or
 * mining succeeded. Keep the same pending receipt and reconcile it by syncing.
 */
const ZEBRA_ALREADY_MINED =
  "any transaction with the same effects will be rejected from the mempool until a chain reset: transaction was committed to the best chain";

export function isDuplicateBroadcast(reason: string): boolean {
  const text = reason.trim();
  if (text.toLowerCase() === ZEBRA_ALREADY_MINED) return true;
  return /^(?:txn-already-(?:in-mempool|known)|transaction is already in the mempool|transaction already in mempool|transaction already exists in (?:the )?mempool|transaction dropped because it is already queued for download|transaction already in (?:the )?block ?chain|already exists in (?:the )?mempool|already in (?:the )?(?:mempool|block ?chain)|transaction was committed to the best chain)\b/i
    .test(text);
}

/** A loopback bridge's `/lwd/sendraw` error body: the node's refusal, if it is one. */
function bridgeRejection(body: { error?: string; rejected?: { code?: number; message?: string } }): BroadcastRejection | null {
  if (body.rejected && typeof body.rejected.message === "string") {
    return new BroadcastRejection(Number(body.rejected.code ?? -1), body.rejected.message);
  }
  // Bridges before the structured field: the engine's BroadcastRejected text.
  const old = /^broadcast rejected \((-?\d+)\): ([\s\S]*)$/.exec(body.error ?? "");
  return old ? new BroadcastRejection(Number(old[1]), old[2] ?? "") : null;
}

/** Only read-only transport outages qualify for scan reconnects. */
export function isTransientLightServerError(error: unknown): boolean {
  if (error instanceof TransientLightServerError) return true;
  if (error instanceof TypeError) return true; // fetch or body-stream network failure
  if (!(error instanceof Error)) return false;
  if (error.name === "NetworkError" || error.name === "TimeoutError") return true;
  return /grpc-status:\s*(?:4|14)\b|grpc-web status (?:4|14)\b|grpc-web \w+: HTTP 50[234]\b/i.test(error.message);
}

function isRequestCancellation(error: unknown): boolean {
  return error instanceof Error && (error.name === "AbortError" || error.name === "TimeoutError");
}

function isRetryableBlockFetch(error: unknown): boolean {
  if (isRequestCancellation(error)) return false;
  if (error instanceof TransientLightServerError) return true;
  // Fetch and response-body network failures are TypeError in Chromium/Node
  // and may be NetworkError in other browsers. Abort/timeout and decoding or
  // application errors must not start another download.
  return error instanceof TypeError || (error instanceof Error && error.name === "NetworkError");
}

function retryDelay(ms: number, signal?: AbortSignal): Promise<void> {
  signal?.throwIfAborted();
  return new Promise((resolve, reject) => {
    const cleanup = () => { clearTimeout(timer); signal?.removeEventListener("abort", onAbort); };
    const onAbort = () => { cleanup(); reject(signal!.reason); };
    const timer = setTimeout(() => { cleanup(); resolve(); }, ms);
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

/**
 * How long a compact-block response may go without delivering a byte. Not a
 * total deadline: a slow link that keeps delivering is never cut off.
 */
const BLOCK_IDLE_MS = 120_000;

/** Aborts when `touch` has not been called for `ms`. */
function idleWatch(ms: number) {
  const controller = new AbortController();
  let stalled = false;
  const fire = () => {
    stalled = true;
    controller.abort(new DOMException(`no data for ${ms} ms`, "TimeoutError"));
  };
  let timer = setTimeout(fire, ms);
  return {
    signal: controller.signal,
    touch() { clearTimeout(timer); timer = setTimeout(fire, ms); },
    stalled: () => stalled,
    stop() { clearTimeout(timer); },
  };
}

/** Whole body, touching `idle` per chunk and giving up as soon as `signal` aborts. */
async function readBody(r: Response, signal: AbortSignal, touch: () => void): Promise<Uint8Array> {
  if (!r.body) return new Uint8Array(await r.arrayBuffer());
  const reader = r.body.getReader();
  const aborted = new Promise<never>((_, reject) => {
    if (signal.aborted) reject(signal.reason);
    signal.addEventListener("abort", () => reject(signal.reason), { once: true });
  });
  void aborted.catch(() => {});
  const parts: Uint8Array[] = [];
  try {
    for (;;) {
      const step = await Promise.race([reader.read(), aborted]);
      if (step.done) break;
      parts.push(step.value);
      touch();
    }
  } catch (error) {
    void reader.cancel().catch(() => {});
    throw error;
  }
  return concatBytes(parts);
}

function requestSignal(signal: AbortSignal | undefined, timeoutMs: number): AbortSignal {
  return signal ? AbortSignal.any([signal, AbortSignal.timeout(timeoutMs)]) : AbortSignal.timeout(timeoutMs);
}

function responseError(status: number, message: string): Error {
  return [502, 503, 504].includes(status) ? new TransientLightServerError(message) : new Error(message);
}

/** Loopback `z-wallet serve` compact-block pipe (`GET /lwd/tip`, `GET /lwd/blocks`). */
export function httpLwdTransport(
  proxyBase: string,
  network: Network,
  server?: string,
  validatorRpc?: string,
  token?: string,
  opts?: { blockIdleMs?: number; transparent?: boolean; allPools?: boolean },
): BlockTransport {
  const blockIdleMs = opts?.blockIdleMs ?? BLOCK_IDLE_MS;
  const { base, token: auth } = splitProxyAuth(proxyBase, token);
  const authed = (init?: RequestInit): RequestInit => {
    if (!auth) return { ...init, ...transportFetchOptions };
    const headers = new Headers(init?.headers);
    headers.set("authorization", `Bearer ${auth}`);
    return { ...init, ...transportFetchOptions, headers };
  };
  const qs = (extra: string) => {
    const p = new URLSearchParams();
    p.set("network", network);
    if (server?.trim()) p.set("server", server.trim());
    if (validatorRpc?.trim()) p.set("rpc", validatorRpc.trim());
    return extra ? `${extra}&${p}` : `${p}`;
  };
  const loopback = isLoopbackUrl(base);
  const isPipe = looksLikeLwdPipe(base);
  const allowT = opts?.transparent ?? (loopback && allowsTransparentQuery(server));
  let mempoolUnsupported = false;
  return {
    transparentBlocks: isPipe ? (start, end, signal) =>
      httpLwdTransport(proxyBase, network, server, validatorRpc, token, { ...opts, allPools: true }).blocks(start, end, signal) : undefined,
    kind: isPipe ? "lwd-pipe" : "http-lwd",
    label: isPipe ? `zaino-pipe ${base}` : `${base}/lwd (${network})`,
    tip: async (signal) => {
      const r = await fetch(`${base}/lwd/tip?${qs("")}`, authed({ signal: requestSignal(signal, 10_000) }));
      const j = (await r.json().catch((error) => {
        if (r.ok) throw error;
        return {};
      })) as { tip?: number; error?: string };
      if (!r.ok) throw responseError(r.status, explainBridgeAuthError(r.status, j.error) || j.error || `${r.status} /lwd/tip`);
      if (typeof j.tip !== "number") throw new Error("no tip");
      return j.tip;
    },
    blocks: async (start, end, signal) => {
      const fetchRange = async (a: number, b: number) => {
        const url = `${base}/lwd/blocks?${qs(`start=${a}&end=${b}${opts?.allPools ? "&allPools=1" : ""}`)}`;
        for (let attempt = 0; ; attempt++) {
          signal?.throwIfAborted();
          // A stalled pipe used to hang this sync, and every sync queued behind
          // it, forever. Stalls become a retryable outage instead.
          const idle = idleWatch(blockIdleMs);
          const both = signal ? AbortSignal.any([signal, idle.signal]) : idle.signal;
          try {
            const r = await fetch(url, authed({ signal: both }));
            idle.touch();
            if (!r.ok) {
              const j = (await r.json().catch((error) => {
                if (isRequestCancellation(error)) throw error;
                return {};
              })) as { error?: unknown } | null;
              signal?.throwIfAborted();
              const detail = typeof j?.error === "string" ? j.error : undefined;
              const message = explainBridgeAuthError(r.status, detail) || detail || `${r.status} /lwd/blocks`;
              throw responseError(r.status, message);
            }
            // Nothing escapes until the complete body has arrived. A truncated
            // response is discarded and the SAME range is retried; never stitch
            // a partial prefix to a second response or retry engine validation.
            const bytes = await readBody(r, both, idle.touch);
            signal?.throwIfAborted();
            return bytes;
          } catch (caught) {
            signal?.throwIfAborted();
            const error = idle.stalled()
              ? new TransientLightServerError(`compact-block download stalled: no data for ${Math.round(blockIdleMs / 1000)}s`)
              : caught;
            if (!isRetryableBlockFetch(error) || attempt >= BLOCK_FETCH_RETRY_MS.length) throw error;
            await retryDelay(BLOCK_FETCH_RETRY_MS[attempt], signal);
          } finally {
            idle.stop();
          }
        }
      };
      if (!isPipe || end - start + 1 <= PIPE_HTTP_MAX) {
        return fetchRange(start, end);
      }
      const parts: Uint8Array[] = [];
      for (let h = start; h <= end; h += PIPE_HTTP_MAX) {
        parts.push(await fetchRange(h, Math.min(end, h + PIPE_HTTP_MAX - 1)));
      }
      return concatBytes(parts);
    },
    blockStream: isPipe
      ? async (start, end, page, onPage) => {
          for (let h = start; h <= end; ) {
            const last = Math.min(end, h + PIPE_HTTP_MAX - 1);
            const r = await fetch(`${base}/lwd/blocks?${qs(`start=${h}&end=${last}`)}`, authed());
            if (!r.ok) {
              const j = (await r.json().catch(() => ({}))) as { error?: string };
              throw new Error(explainBridgeAuthError(r.status, j.error) || j.error || `${r.status} /lwd/blocks`);
            }
            if (!r.body) throw new Error("lwd-pipe returned no body");
            let got = 0;
            await consumeDelimitedStream(r.body, page, async (blob, n) => {
              got += n;
              await onPage(blob, n);
            });
            if (got !== last - h + 1) throw new Error(`lwd-pipe incomplete range ${h}..=${last}: received ${got} blocks`);
            h = last + 1;
          }
        }
      : undefined,
    info: isPipe
      ? async () => {
          const r = await fetch(`${base}/lwd/info?${qs("")}`, authed({ signal: AbortSignal.timeout(10_000) }));
          const j = (await r.json().catch(() => ({}))) as {
            chain?: string;
            blockHeight?: number;
            vendor?: string;
            version?: string;
            protocolVersion?: string;
            transparentCompact?: boolean;
            error?: string;
          };
          if (!r.ok) throw new Error(j.error || `${r.status} /lwd/info`);
          return {
            chain: j.chain ?? "",
            blockHeight: j.blockHeight,
            vendor: j.vendor,
            version: j.version,
            protocolVersion: j.protocolVersion,
            transparentCompact: j.transparentCompact,
          };
        }
      : undefined,
    submit: async (rawHex) => {
      const r = await fetch(
        `${base}/lwd/sendraw?${qs("")}`,
        authed({
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ hex: rawHex }),
          // Past the validator RPC's own 120 s read limit; a timeout is an
          // unknown outcome, which keeps the send pending for rebroadcast.
          signal: AbortSignal.timeout(150_000),
        }),
      );
      const j = (await r.json().catch(() => ({}))) as {
        txid?: string;
        error?: string;
        rejected?: { code?: number; message?: string };
      };
      if (!r.ok) throw bridgeRejection(j) ?? new Error(j.error || `${r.status} /lwd/sendraw`);
      return j.txid || "ok";
    },
    utxos: allowT
      ? async (addresses, startHeight = 0, signal) => {
          const addrs = addresses.map((a) => a.trim()).filter(Boolean);
          if (!addrs.length) return [];
          const extra = `addresses=${encodeURIComponent(addrs.join(","))}&start=${startHeight}`;
          const r = await fetch(`${base}/lwd/utxos?${qs(extra)}`, authed({ signal: requestSignal(signal, 30_000) }));
          const j = (await r.json().catch(() => ({}))) as {
            utxos?: TransparentUtxo[];
            error?: string;
          };
          if (!r.ok) throw responseError(r.status, j.error || `${r.status} /lwd/utxos`);
          return j.utxos ?? [];
        }
      : undefined,
    mempool: async () => {
      if (mempoolUnsupported) return [];
      let r: Response;
      try {
        r = await fetch(`${base}/lwd/mempool?${qs("")}`, authed({ signal: AbortSignal.timeout(30_000) }));
      } catch (error) {
        // Mempool is best effort; a stalled answer must not hold the sync.
        if (!isRequestCancellation(error)) throw error;
        console.warn("mempool timed out");
        return [];
      }
      if (r.status === 404 || r.status === 501) {
        mempoolUnsupported = true;
        return [];
      }
      const j = (await r.json().catch(() => ({}))) as {
        txs?: Array<{ txid?: string; hex?: string }>;
        error?: string;
      };
      if (!r.ok) {
        // Transient RPC failure — empty, not a hard send/sync error.
        console.warn("mempool unavailable:");
        return [];
      }
      return (j.txs ?? []).filter((t) => t.hex).map((t) => ({ txid: t.txid, hex: t.hex as string }));
    },
    tx: async (txid, signal) => {
      const r = await fetch(`${base}/lwd/tx?${qs(`txid=${encodeURIComponent(txid)}`)}`, authed({ signal: requestSignal(signal, 60_000), priority: "low" }));
      const j = (await r.json().catch(() => ({}))) as { hex?: string; error?: string };
      if (!r.ok) throw new Error(j.error || `${r.status} /lwd/tx`);
      if (!j.hex) throw new Error("no tx hex");
      return j.hex;
    },
    treeState: async (height, signal) => {
      const r = await fetch(`${base}/lwd/treestate?${qs(`height=${height}`)}`, authed({ signal: requestSignal(signal, 30_000) }));
      const j = (await r.json().catch(() => ({}))) as {
        height?: number;
        hash?: string;
        saplingTree?: string;
        orchardTree?: string;
        ironwoodTree?: string;
        error?: string;
      };
      if (!r.ok) throw responseError(r.status, j.error || `${r.status} /lwd/treestate`);
      if (typeof j.height !== "number" || !j.hash) throw new Error("no tree state");
      return j as {
        height: number;
        hash: string;
        saplingTree?: string;
        orchardTree?: string;
        ironwoodTree?: string;
      };
    },
    subtreeRoots: async (protocol, startIndex = 0, signal, maxEntries = 0) => {
      const start = Math.max(0, startIndex | 0);
      const limit = Math.max(0, maxEntries | 0);
      const r = await fetch(
        `${base}/lwd/subtreeroots?${qs(
          `protocol=${encodeURIComponent(protocol)}&startIndex=${start}${limit ? `&maxEntries=${limit}` : ""}`,
        )}`,
        // Zaino spends up to a second per root, so callers page with
        // maxEntries. A short page limit keeps one stalled page from holding
        // the birthday scan; the caller retries it.
        authed({ signal: requestSignal(signal, isPipe ? 15_000 : 180_000) }),
      );
      const j = (await r.json().catch(() => ({}))) as {
        roots?: Array<{ completingHeight?: number; rootHash?: string }>;
        error?: string;
      };
      if (!r.ok) throw responseError(r.status, j.error || `${r.status} /lwd/subtreeroots`);
      return (j.roots ?? [])
        .filter((x) => x.rootHash)
        .map((x) => ({ completingHeight: x.completingHeight ?? 0, rootHash: x.rootHash as string }));
    },
    mine: loopback
      ? async (blocks) => {
      const r = await fetch(
        `${base}/lwd/mine?${qs("")}`,
        authed({
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ blocks: Math.max(1, blocks | 0) }),
        }),
      );
      const j = (await r.json().catch(() => ({}))) as { mined?: number; error?: string };
      if (!r.ok) throw new Error(j.error || `${r.status} /lwd/mine`);
      return { mined: j.mined ?? blocks };
    }
      : undefined,
  };
}

function readVarint(buf: Uint8Array, i: number): [number, number] {
  let n = 0;
  let shift = 0;
  while (i < buf.length) {
    const b = buf[i++];
    n += (b & 0x7f) << shift;
    if ((b & 0x80) === 0) break;
    shift += 7;
    if (shift > 35) break;
  }
  return [n >>> 0, i];
}

function protoString(buf: Uint8Array, field: number): string | undefined {
  const bytes = protoBytes(buf, field);
  return bytes === undefined ? undefined : new TextDecoder().decode(bytes);
}

function protoVarint(buf: Uint8Array, field: number): number | undefined {
  let i = 0;
  while (i < buf.length) {
    const [key, next] = readVarint(buf, i);
    i = next;
    const f = key >> 3;
    const wire = key & 7;
    if (wire === 0) {
      const [n, j] = readVarint(buf, i);
      i = j;
      if (f === field) return n;
    } else if (wire === 2) {
      const [len, j] = readVarint(buf, i);
      i = j + len;
    } else if (wire === 1) {
      i += 8;
    } else if (wire === 5) {
      i += 4;
    } else {
      break;
    }
  }
  return undefined;
}

export type GrpcWebTransportOpts = {
  /**
   * Look up this wallet's transparent address with `GetAddressUtxos`, so
   * t-address deposits show up and can be shielded. The server learns the
   * address. Default false: public servers are shield-only.
   */
  transparent?: boolean;
  /**
   * The configured gateway implements the optional /zstack/memos extension.
   * Default false, including loopback. Standard gRPC-Web servers do not
   * advertise this capability through GetLightdInfo.
   */
  sharedMemos?: boolean;
};

/**
 * Browser gRPC-Web CompactTxStreamer (needs CORS on the light server). No
 * bridge token. Scans, broadcasts (`SendTransaction`) and fetches full
 * transactions for memos (`GetTransaction`).
 */
export function grpcWebTransport(lwdUrl: string, opts: GrpcWebTransportOpts = {}): BlockTransport {
  const base = lwdUrl.replace(/\/$/, "");
  let sharedMemosUnsupported = false;
  async function call(method: string, body: Uint8Array, timeoutMs = 15_000, signal?: AbortSignal): Promise<GrpcMessages> {
    const deadline = AbortSignal.timeout(timeoutMs);
    try {
      const r = await fetch(`${base}${STREAMER}/${method}`, {
        ...transportFetchOptions,
        method: "POST",
        headers: {
          "content-type": "application/grpc-web+proto",
          "x-grpc-web": "1",
          accept: "application/grpc-web+proto",
        },
        body: frameGrpc(body) as unknown as BodyInit,
        signal: signal ? AbortSignal.any([signal, deadline]) : deadline,
        // keepalive bodies are capped at 64 KiB; large transactions must not use it.
        keepalive: body.length < 32_768,
      });
      if (!r.ok) throw responseError(r.status, `grpc-web ${method}: HTTP ${r.status}`);
      // A trailers-only error arrives as HTTP 200 with the status in the headers.
      const headerStatus = r.headers.get("grpc-status");
      grpcStatusError(headerStatus, r.headers.get("grpc-message"));
      const messages = parseGrpcWeb(new Uint8Array(await r.arrayBuffer()));
      if (headerStatus && !messages.grpcStatus) messages.grpcStatus = headerStatus.trim();
      return messages;
    } catch (error) {
      // Body reads can throw AbortError even when the deadline caused the abort.
      // Keep real cancellation distinct so the scan's outage clock can retry a
      // slow response without restarting work the user explicitly cancelled.
      signal?.throwIfAborted();
      if (deadline.aborted) throw deadline.reason;
      throw error;
    }
  }
  return {
    transparentBlocks: async (start, end, signal) => toDelimited(
      await call("GetBlockRange", encodeBlockRange(start, end, true), GRPC_WEB_BLOCK_TIMEOUT_MS, signal)),
    sharedMemos: opts.sharedMemos === true ? async (start, end, signal) => {
      signal?.throwIfAborted();
      if (sharedMemosUnsupported) return null;
      const requestAbort = requestSignal(signal, 180_000);
      const r = await fetch(`${base}/zstack/memos?start=${start}&end=${end}`, {
        ...transportFetchOptions, priority: "low", signal: requestAbort,
      });
      requestAbort.throwIfAborted();
      if ([403, 404, 405, 501].includes(r.status)) {
        sharedMemosUnsupported = true;
        return null;
      }
      if (!r.ok) throw responseError(r.status, "Shared payment notes are temporarily unavailable");
      const reader = r.body?.getReader();
      if (!reader) throw new Error("Empty shared memo response");
      const parts: Uint8Array[] = [];
      let size = 0;
      try {
        for (;;) {
          const { done, value } = await reader.read();
          requestAbort.throwIfAborted();
          if (done) break;
          size += value.length;
          if (size > 64 * 1024 * 1024) throw new Error("Shared memo range exceeds size limit");
          parts.push(value);
        }
      } catch (error) {
        await reader.cancel().catch(() => {});
        throw error;
      } finally { reader.releaseLock(); }
      const bytes = new Uint8Array(size);
      let offset = 0;
      for (const part of parts) { bytes.set(part, offset); offset += part.length; }
      return new TextDecoder().decode(bytes);
    } : undefined,
    kind: "grpc-web",
    label: `grpc-web ${base}`,
    tip: async (signal) => {
      const msgs = await call("GetLatestBlock", new Uint8Array(), 15_000, signal);
      if (!msgs[0]) throw new Error("empty GetLatestBlock");
      return decodeBlockIdHeight(msgs[0]);
    },
    blocks: async (start, end, signal) => {
      // Range sizes are already capped at 1k by sync tuning. Do not split a
      // timed-out request into parallel subranges outside the scan's shared
      // outage clock; prefetch will abort and restart from the in-memory cursor.
      const msgs = await call("GetBlockRange", encodeBlockRange(start, end), GRPC_WEB_BLOCK_TIMEOUT_MS, signal);
      return toDelimited(msgs);
    },
    info: async () => {
      const msgs = await call("GetLightdInfo", new Uint8Array());
      if (!msgs[0]) throw new Error("empty GetLightdInfo");
      return {
        protocolVersion: protoString(msgs[0], 18),
        transparentCompact: true,
        version: protoString(msgs[0], 1),
        vendor: protoString(msgs[0], 2),
        chain: protoString(msgs[0], 4) ?? "",
        consensusBranchId: protoString(msgs[0], 6),
        blockHeight: protoVarint(msgs[0], 7),
      };
    },
    treeState: async (height, signal) => {
      const msgs = await call("GetTreeState", encodeBlockId(height), 15_000, signal);
      if (!msgs[0]) throw new Error("empty GetTreeState");
      const hash = protoString(msgs[0], 3);
      const h = protoVarint(msgs[0], 2);
      if (!hash || h === undefined) throw new Error("no tree state");
      return {
        network: protoString(msgs[0], 1),
        height: h,
        hash,
        time: protoVarint(msgs[0], 4),
        saplingTree: protoString(msgs[0], 5),
        orchardTree: protoString(msgs[0], 6),
        ironwoodTree: protoString(msgs[0], 7),
      };
    },
    subtreeRoots: async (protocol, startIndex = 0, signal, maxEntries = 0) => {
      const msgs = await call(
        "GetSubtreeRoots",
        encodeGetSubtreeRoots(shieldedProtocol(protocol), startIndex, maxEntries),
        180_000,
        signal,
      );
      return msgs.map(decodeSubtreeRoot);
    },
    submit: async (rawHex) => {
      // Past a validator's own 120 s limit; a timeout is an unknown outcome,
      // which keeps the send pending for rebroadcast.
      const msgs = await call("SendTransaction", encodeRawTransaction(fromHex(rawHex)), 150_000);
      // Only a completed call counts: a SendResponse and an OK status. A cut
      // stream is an unknown outcome, so the send stays pending.
      if (!msgs[0]) throw new Error("SendTransaction returned no response");
      if (msgs.grpcStatus !== "0") throw new Error("SendTransaction ended without a gRPC status");
      const reply = protoFieldsOf(msgs[0]);
      // errorCode is a protobuf int32. Rejections are negative, encoded as a
      // sign-extended varint that an unsigned decoder turns into a huge number.
      const code = protoInt32(msgs[0], 1);
      const text = reply.find((f) => f.field === 2 && f.value instanceof Uint8Array)?.value as Uint8Array | undefined;
      const message = text ? new TextDecoder().decode(text) : "";
      if (code) throw new BroadcastRejection(code, message);
      return message || "ok";
    },
    tx: async (txid, signal) => {
      // Servers take TxFilter.hash in wire order (display order reversed); some
      // older ones want display order. Try both, like the native engine.
      const display = fromHex(txid);
      let last: unknown;
      for (const hash of [display.slice().reverse(), display]) {
        signal?.throwIfAborted();
        try {
          const msgs = await call("GetTransaction", encodeTxFilterHash(hash), 60_000, signal);
          const data = msgs[0] ? protoBytes(msgs[0], 1) : undefined;
          if (data && data.length) return toHex(data);
        } catch (e) {
          last = e;
        }
      }
      throw last instanceof Error ? last : new Error(`GetTransaction ${txid}: not found`);
    },
    utxos: opts.transparent
      ? async (addresses, startHeight = 0, signal) => {
          const addrs = addresses.map((a) => a.trim()).filter(Boolean);
          if (!addrs.length) return [];
          const msgs = await call("GetAddressUtxos", encodeGetAddressUtxos(addrs, startHeight), 30_000, signal);
          return msgs.length ? decodeAddressUtxos(msgs[0]!) : [];
        }
      : undefined,
  };
}

/** Every field of one protobuf message; varints as numbers (exact to 2^53). */
function protoFieldsOf(buf: Uint8Array, strict = false): Array<{ field: number; value: number | Uint8Array }> {
  const out: Array<{ field: number; value: number | Uint8Array }> = [];
  let i = 0;
  while (i < buf.length) {
    const [key, k] = readVarint64(buf, i, strict);
    i = k;
    const field = Math.floor(key / 8);
    const wire = key % 8;
    if (wire === 0) {
      const [n, j] = readVarint64(buf, i, strict);
      i = j;
      out.push({ field, value: n });
    } else if (wire === 2) {
      const [len, j] = readVarint64(buf, i, strict);
      if (strict && j + len > buf.length) throw new Error("invalid protobuf field length");
      out.push({ field, value: buf.subarray(j, j + len) });
      i = j + len;
    } else if (wire === 1) {
      i += 8;
    } else if (wire === 5) {
      i += 4;
    } else {
      if (strict) throw new Error("invalid protobuf wire type");
      break;
    }
    if (strict && i > buf.length) throw new Error("truncated protobuf field");
  }
  return out;
}

/** Field `field` as a protobuf int32, or undefined when the field is absent. */
export function protoInt32(buf: Uint8Array, field: number): number | undefined {
  let i = 0;
  while (i < buf.length) {
    const [key, keyEnd] = readVarintBig(buf, i);
    i = keyEnd;
    const id = Number(key >> 3n);
    const wire = Number(key & 7n);
    if (wire === 0) {
      const [value, next] = readVarintBig(buf, i);
      i = next;
      if (id === field) return Number(BigInt.asIntN(32, value));
    } else if (wire === 2) {
      const [len, next] = readVarintBig(buf, i);
      i = next + Number(len);
    } else if (wire === 1) {
      i += 8;
    } else if (wire === 5) {
      i += 4;
    } else {
      break;
    }
  }
  return undefined;
}

function readVarintBig(buf: Uint8Array, i: number): [bigint, number] {
  let n = 0n;
  let shift = 0n;
  while (i < buf.length && shift <= 63n) {
    const byte = BigInt(buf[i++]!);
    n |= (byte & 0x7fn) << shift;
    if ((byte & 0x80n) === 0n) return [n, i];
    shift += 7n;
  }
  return [n, i];
}

function readVarint64(buf: Uint8Array, i: number, strict = false): [number, number] {
  let n = 0;
  let mul = 1;
  while (i < buf.length) {
    const b = buf[i++]!;
    n += (b & 0x7f) * mul;
    if ((b & 0x80) === 0) {
      if (strict && !Number.isSafeInteger(n)) throw new Error("unsafe protobuf integer");
      return [n, i];
    }
    mul *= 128;
    if (mul > 2 ** 63) break;
  }
  if (strict) throw new Error("truncated or oversized protobuf varint");
  return [n, i];
}

/** Length-delimited protobuf field. Does not spread the payload into an array literal. */
function lengthDelimited(field: number, bytes: Uint8Array): Uint8Array {
  const len = Uint8Array.from(encodeVarint(bytes.length));
  const out = new Uint8Array(1 + len.length + bytes.length);
  out[0] = (field << 3) | 2;
  out.set(len, 1);
  out.set(bytes, 1 + len.length);
  return out;
}

/** `RawTransaction`: data=1. */
function encodeRawTransaction(data: Uint8Array): Uint8Array {
  return lengthDelimited(1, data);
}

/** `TxFilter`: block=1, index=2 (varint), hash=3 (bytes). */
function encodeTxFilterHash(hash: Uint8Array): Uint8Array {
  return lengthDelimited(3, hash);
}

/** `GetAddressUtxosArg`: addresses=1 (repeated), startHeight=2. */
function encodeGetAddressUtxos(addresses: string[], startHeight: number): Uint8Array {
  const enc = new TextEncoder();
  const parts = addresses.map((address) => lengthDelimited(1, enc.encode(address)));
  if (startHeight > 0) parts.push(concatBytes([Uint8Array.of(0x10), Uint8Array.from(encodeVarint(startHeight))]));
  return concatBytes(parts);
}

/**
 * `GetAddressUtxosReplyList.addressUtxos` (1), per lightwallet-protocol service.proto:
 * txid=1, index=2, script=3, valueZat=4, height=5, address=6. The txid stays
 * in wire order, as the loopback pipe reports it.
 * https://github.com/zcash/lightwallet-protocol/blob/main/walletrpc/service.proto
 */
export function decodeAddressUtxos(list: Uint8Array): TransparentUtxo[] {
  const out: TransparentUtxo[] = [];
  for (const entry of protoFieldsOf(list)) {
    if (entry.field !== 1 || !(entry.value instanceof Uint8Array)) continue;
    const u: TransparentUtxo = { txid: "", index: 0, script: "", valueZat: 0, height: 0, address: "" };
    for (const f of protoFieldsOf(entry.value)) {
      if (f.field === 1 && f.value instanceof Uint8Array) u.txid = toHex(f.value);
      else if (f.field === 2 && typeof f.value === "number") u.index = f.value;
      else if (f.field === 3 && f.value instanceof Uint8Array) u.script = toHex(f.value);
      else if (f.field === 4 && typeof f.value === "number") u.valueZat = f.value;
      else if (f.field === 5 && typeof f.value === "number") u.height = f.value;
      else if (f.field === 6 && f.value instanceof Uint8Array) u.address = new TextDecoder().decode(f.value);
    }
    if (u.txid && u.valueZat > 0) out.push(u);
  }
  return out;
}

function fromHex(hex: string): Uint8Array {
  const clean = hex.trim().replace(/^0x/i, "");
  if (clean.length % 2 || /[^0-9a-f]/i.test(clean)) throw new Error("expected hexadecimal");
  const out = new Uint8Array(clean.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  return out;
}

function decodeBlockIdHeight(buf: Uint8Array): number {
  let i = 0;
  while (i < buf.length) {
    const key = buf[i++];
    const field = key >> 3;
    const wire = key & 7;
    if (wire === 0) {
      let n = 0;
      let shift = 0;
      while (i < buf.length) {
        const b = buf[i++];
        n += (b & 0x7f) << shift;
        if ((b & 0x80) === 0) break;
        shift += 7;
      }
      if (field === 1) return n;
    } else if (wire === 2) {
      let len = 0;
      let shift = 0;
      while (i < buf.length) {
        const b = buf[i++];
        len += (b & 0x7f) << shift;
        if ((b & 0x80) === 0) break;
        shift += 7;
      }
      i += len;
    } else {
      break;
    }
  }
  throw new Error("GetLatestBlock missing height");
}
