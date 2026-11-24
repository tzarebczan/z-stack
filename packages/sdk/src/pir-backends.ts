/**
 * Pluggable PIR backends (Enhance, Transparent, Status) over any `NetworkHop`.
 *
 * Each PIR product is independently configurable: point all of them at one
 * unified server (e.g., Valar `wallet-pir.valargroup.dev` or a self-hosted
 * `wallet-pir`) or split across operators (e.g., Coffer self-hosted Enhance
 * + Valar Transparent), each routed over any `NetworkHop` (OHTTP, Nym+OHTTP,
 * Tor+OHTTP, or local loopback).
 */

import type { NetworkHop } from "./hops";
import type { PrivateEnhanceRequest, PrivateEnhanceTransport } from "./private-enhance";
import type { PrivateTransparentTransport } from "./private-transparent";

export type EnhancePirProtocol = "v7-q48";
export type TransparentPirProtocol = "transparent-shard-v12";

/** Unified optional PIR bundle attached to a wallet or `BlockTransport`. */
export interface PirBackends {
  enhance?: PrivateEnhanceTransport;
  transparent?: PrivateTransparentTransport;
}

function normalizeBaseUrl(baseUrl: string): string {
  const trimmed = baseUrl.trim().replace(/\/+$/, "");
  if (
    !trimmed ||
    !(
      trimmed.startsWith("https://") ||
      trimmed.startsWith("http://127.0.0.1:") ||
      trimmed.startsWith("http://[::1]:") ||
      trimmed.startsWith("/")
    )
  ) {
    throw new Error("PIR base URL must be HTTPS, loopback HTTP, or an inner OHTTP route prefix.");
  }
  return trimmed;
}

function asPirStatus(status: number): 200 | 409 | 410 | 429 | 503 {
  if (status === 200 || status === 409 || status === 410 || status === 429 || status === 503) {
    return status;
  }
  throw new Error(`Unexpected PIR HTTP status ${status}.`);
}

function encodeBase64(bytes: Uint8Array): string {
  const chunk = 0x8000;
  let binary = "";
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

function decodeBase64(b64: string): Uint8Array {
  const binary = atob(b64);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    out[i] = binary.charCodeAt(i);
  }
  return out;
}

function formatTransparentTable(raw: unknown): string {
  if (!raw || typeof raw !== "object" || !("kind" in raw)) {
    throw new Error("Invalid Transparent PIR table selector.");
  }
  const table = raw as { kind: unknown; child?: unknown };
  if (table.kind === "directory") return "directory";
  if (
    table.kind === "pages" &&
    typeof table.child === "number" &&
    Number.isSafeInteger(table.child) &&
    table.child >= 0 &&
    table.child < 32
  ) {
    return `pages-${table.child}`;
  }
  throw new Error("Invalid Transparent PIR table selector.");
}

function validateRevision(raw: unknown): string {
  if (typeof raw !== "string" || !/^[A-Za-z0-9._-]{1,128}$/.test(raw)) {
    throw new Error("Invalid Transparent PIR revision.");
  }
  return raw;
}

/**
 * Create a hop-backed Enhance PIR transport targeting any compatible v7/q48 Enhance PIR
 * server (Coffer self-hosted or an OHTTP-proxied origin).
 */
export function createHopEnhancePirTransport(opts: {
  baseUrl: string;
  hop: NetworkHop;
  protocol?: EnhancePirProtocol;
  timeoutMs?: number;
}): PrivateEnhanceTransport {
  const base = normalizeBaseUrl(opts.baseUrl);
  const protocol = opts.protocol ?? "v7-q48";
  if (protocol !== "v7-q48") {
    throw new Error(`Unsupported Enhance PIR protocol: ${String(protocol)}`);
  }
  const prefix = "/v1/enhance";
  const timeoutMs = opts.timeoutMs ?? 45_000;

  return {
    async exchange(request: PrivateEnhanceRequest, signal: AbortSignal) {
      if (request.kind === "init") {
        const res = await opts.hop.exchange(
          {
            method: "GET",
            target: `${base}${prefix}/init`,
            role: "pir",
            maxResponseBytes: 1024 * 1024,
            timeoutMs,
            idempotent: true,
          },
          signal,
        );
        return { status: asPirStatus(res.status), body: res.body };
      }
      if (request.kind === "session") {
        if (!/^[0-9a-f]{64}$/.test(request.sessionId)) {
          throw new Error("Invalid Enhance PIR session ID.");
        }
        const res = await opts.hop.exchange(
          {
            method: "GET",
            target: `${base}${prefix}/session/${request.sessionId}`,
            role: "pir",
            maxResponseBytes: 256 * 1024,
            timeoutMs,
            idempotent: true,
          },
          signal,
        );
        return { status: asPirStatus(res.status), body: res.body };
      }
      const res = await opts.hop.exchange(
        {
          method: "POST",
          target: `${base}${prefix}/query`,
          headers: {
            "content-type": "application/octet-stream",
            "x-enhance-anchor-height": String(request.anchor.height),
            "x-enhance-anchor-hash": request.anchor.hash,
          },
          body: request.body,
          role: "pir",
          maxResponseBytes: 34 * 1024,
          timeoutMs,
          idempotent: true,
        },
        signal,
      );
      return { status: asPirStatus(res.status), body: res.body };
    },
  };
}

/**
 * Create a hop-backed Transparent v12 PIR transport targeting any `transparent-shard-v12`
 * server (e.g., Valar `https://transparent-pir.valargroup.dev` or self-hosted) through
 * any `NetworkHop` (OHTTP, Nym+OHTTP, Tor+OHTTP, or local loopback).
 *
 * Matches `z_transparent_pir_v2::types::{Operation, Table, Reply}` and the upstream
 * `/v2/shards/...` wire protocol identically.
 */
export function createHopTransparentPirTransport(opts: {
  baseUrl: string;
  sourceId: string;
  hop: NetworkHop;
  timeoutMs?: number;
}): PrivateTransparentTransport {
  const base = normalizeBaseUrl(opts.baseUrl);
  if (!/^[0-9a-f]{64}$/.test(opts.sourceId)) {
    throw new Error("Transparent PIR sourceId must be 64 lowercase hex characters.");
  }
  const timeoutMs = opts.timeoutMs ?? 60_000;
  const decoder = new TextDecoder("utf-8", { fatal: true });
  const encoder = new TextEncoder();

  function parseOp(raw: Uint8Array): { kind: string; [k: string]: unknown } {
    const parsed = JSON.parse(decoder.decode(raw)) as unknown;
    if (!parsed || typeof parsed !== "object" || !("kind" in parsed) || typeof parsed.kind !== "string") {
      throw new Error("Invalid Transparent PIR operation payload.");
    }
    return parsed as { kind: string; [k: string]: unknown };
  }

  return {
    sourceId: opts.sourceId,
    async exchange(request, signal) {
      const op = parseOp(request.operation);
      let method: "GET" | "POST" = "GET";
      let path = "";
      let body: Uint8Array | undefined;
      let maxResponseBytes = 24 * 1024 * 1024;

      switch (op.kind) {
        case "init":
          path = "/v2/shards/init";
          maxResponseBytes = 1024 * 1024;
          break;
        case "map":
          path = "/v2/filters/shards";
          maxResponseBytes = 2 * 1024 * 1024;
          break;
        case "filter": {
          const shard = Number(op.shard_id);
          if (!Number.isSafeInteger(shard) || shard < 0) throw new Error("Invalid shard_id.");
          path = `/v2/filters/shards/${shard}/filter`;
          maxResponseBytes = 16 * 1024 * 1024;
          break;
        }
        case "manifest": {
          const shard = Number(op.shard_id);
          const revision = validateRevision(op.revision);
          if (!Number.isSafeInteger(shard) || shard < 0) throw new Error("Invalid shard_id.");
          path = `/v2/shards/${shard}/revisions/${revision}/manifest`;
          maxResponseBytes = 1024 * 1024;
          break;
        }
        case "setup": {
          const shard = Number(op.shard_id);
          const revision = validateRevision(op.revision);
          const table = formatTransparentTable(op.table);
          const segment = Number(op.segment);
          if (!Number.isSafeInteger(shard) || shard < 0 || !Number.isSafeInteger(segment) || segment < 0) {
            throw new Error("Invalid setup parameters.");
          }
          path = `/v2/shards/${shard}/revisions/${revision}/setup/${table}/${segment}`;
          maxResponseBytes = 24 * 1024 * 1024;
          break;
        }
        case "query": {
          const shard = Number(op.shard_id);
          const revision = validateRevision(op.revision);
          const table = formatTransparentTable(op.table);
          if (!Number.isSafeInteger(shard) || shard < 0 || typeof op.body !== "string") {
            throw new Error("Invalid query parameters.");
          }
          method = "POST";
          path = `/v2/shards/${shard}/revisions/${revision}/query/${table}`;
          body = decodeBase64(op.body);
          maxResponseBytes = 8 * 1024 * 1024;
          break;
        }
        default:
          throw new Error(`Unsupported Transparent PIR operation: ${op.kind}`);
      }

      const res = await opts.hop.exchange(
        {
          method,
          target: `${base}${path}`,
          headers: method === "POST" ? { "content-type": "application/octet-stream" } : undefined,
          body,
          role: "pir",
          maxResponseBytes,
          timeoutMs,
          idempotent: true,
        },
        signal,
      );

      const retryAfterRaw = res.headers["retry-after"];
      const retryAfter =
        retryAfterRaw && /^\d+$/.test(retryAfterRaw.trim())
          ? Math.min(60, Math.max(1, Number(retryAfterRaw.trim())))
          : undefined;

      let reply: Record<string, unknown>;
      if (res.status === 200) {
        reply = { kind: "data", body: encodeBase64(res.body) };
      } else if (res.status === 409) {
        let mapDigestHint: string | null = null;
        try {
          const parsed = JSON.parse(decoder.decode(res.body)) as { map_sha256?: unknown };
          if (typeof parsed.map_sha256 === "string" && /^[0-9a-f]{64}$/.test(parsed.map_sha256)) {
            mapDigestHint = parsed.map_sha256;
          }
        } catch {
          // Optional hint only.
        }
        reply = { kind: "stale", map_digest_hint: mapDigestHint };
      } else if (res.status === 503) {
        reply = retryAfter !== undefined
          ? { kind: "overloaded", retry_after: retryAfter }
          : { kind: "unavailable" };
      } else if (res.status === 502 || res.status === 504) {
        reply = { kind: "overloaded", retry_after: retryAfter ?? 5 };
      } else if (res.status === 421) {
        reply = { kind: "misrouted" };
      } else if (res.status === 400 || res.status === 408 || res.status === 410 || res.status === 411) {
        reply = { kind: "refused" };
      } else {
        reply = { kind: "unavailable" };
      }
      return encoder.encode(JSON.stringify(reply));
    },
  };
}
