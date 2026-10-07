/** Opt-in setup checks. No wallet reads, storage writes, logging or telemetry. */
import { browserWalletActive } from "./wallet-owner";
import { abortable } from "./abort";

export type SetupCheckCode = "browser" | "secure_context" | "webassembly" | "webcrypto" |
  "storage" | "isolation" | "wallet_owner" | "module_worker" | "assets" |
  "asset_missing" | "asset_format" | "integrity_mismatch" | "asset_unavailable" |
  "server" | "shared_memos" | "cancelled";
export type SetupCheck = { code: SetupCheckCode; status: "pass" | "warning" | "fail" | "skipped"; message: string };
export type SetupReport = { ok: boolean; checks: SetupCheck[] };
export interface SetupCheckOptions {
  /** Explicitly fetch and hash the selected bundled engine, or these custom ST assets. */
  assets?: true | { wasmUrl: string; integrityUrl: string };
  /** Start and immediately terminate a tiny module worker; no engine is loaded. */
  worker?: boolean;
  /** App-owned, cancellable public-chain capability probe. Never pass wallet queries. */
  server?: (signal: AbortSignal) => Promise<{ reachable: boolean; sharedMemos: boolean }>;
  signal?: AbortSignal;
  /** Deadline for the entire check. Default 10 seconds; maximum 60 seconds. */
  timeoutMs?: number;
}
const messages = {
  browser: "Run setup checks in browser client code.",
  secure_context: "Use HTTPS or localhost for wallet cryptography and passkeys.",
  webassembly: "This browser needs WebAssembly support.",
  webcrypto: "This browser needs WebCrypto in a secure context.",
  storage: "IndexedDB is unavailable; supply a transactional local storage adapter.",
  isolation: "Threaded mode needs cross-origin isolation and SharedArrayBuffer; single-thread mode remains available.",
  wallet_owner: "A wallet client owns this page's engine. Await close before opening another.",
  module_worker: "Check module-worker support, worker URLs and worker-src CSP.",
  assets: "Engine bytes match their integrity manifest. This does not authenticate your deployment host.",
  asset_missing: "Deploy matching WASM and integrity files; disable app-shell fallback for asset paths.",
  asset_format: "Serve WASM as application/wasm and a valid SHA-256 integrity manifest.",
  integrity_mismatch: "Rebuild and deploy the WASM and manifest from the same release.",
  asset_unavailable: "Check asset hosting, CSP, CORS and the setup-check deadline.",
  server: "Check the configured chain server's availability, CORS and supported transport.",
  shared_memos: "Use shared memo retrieval only when the server advertises that capability.",
  cancelled: "Setup check cancelled.",
} satisfies Record<SetupCheckCode, string>;

class AssetFailure { constructor(readonly code: SetupCheckCode) {} }
async function boundedBody(response: Response, limit: number): Promise<Uint8Array<ArrayBuffer>> {
  if (!response.body) throw new AssetFailure("asset_unavailable");
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > limit) throw new AssetFailure("asset_format");
      chunks.push(value);
    }
  } finally { await reader.cancel().catch(() => {}); reader.releaseLock(); }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
  return bytes;
}
async function probeAssets(assets: NonNullable<SetupCheckOptions["assets"]>, signal: AbortSignal): Promise<void> {
  const threaded = globalThis.crossOriginIsolated === true && typeof SharedArrayBuffer !== "undefined";
  const wasmUrl = assets === true ? (threaded ? new URL("./generated-mt/z_wasm_bg.wasm", import.meta.url) : new URL("./generated/z_wasm_bg.wasm", import.meta.url)).href : assets.wasmUrl;
  const integrityUrl = assets === true ? (threaded ? new URL("./generated-mt/integrity.json", import.meta.url) : new URL("./generated/integrity.json", import.meta.url)).href : assets.integrityUrl;
  const fetchAsset = async (url: string, limit: number) => {
    let response: Response;
    try { response = await fetch(url, { signal, credentials: "omit", redirect: "error", cache: "no-store", referrerPolicy: "no-referrer" }); }
    catch { throw new AssetFailure("asset_unavailable"); }
    if (!response.ok) {
      await response.body?.cancel().catch(() => {});
      throw new AssetFailure(response.status === 404 ? "asset_missing" : "asset_unavailable");
    }
    return { response, bytes: await boundedBody(response, limit) };
  };
  let manifest: unknown;
  try {
    const inline = /^data:application\/json(;base64)?,(.*)$/s.exec(integrityUrl);
    if (inline) {
      if (inline[2].length > 24_000) throw new AssetFailure("asset_format");
      manifest = JSON.parse(inline[1] ? atob(inline[2]) : decodeURIComponent(inline[2]));
    } else manifest = JSON.parse(new TextDecoder().decode((await fetchAsset(integrityUrl, 16_384)).bytes));
  } catch (error) { if (error instanceof AssetFailure) throw error; throw new AssetFailure("asset_format"); }
  const expected = (manifest as { sha256?: unknown } | null)?.sha256;
  if (typeof expected !== "string" || !/^[a-f0-9]{64}$/i.test(expected)) throw new AssetFailure("asset_format");
  const { response, bytes } = await fetchAsset(wasmUrl, 64 * 1024 * 1024);
  if (response.headers.get("content-type")?.split(";")[0].trim() !== "application/wasm" ||
      bytes.length < 8 || bytes[0] !== 0 || bytes[1] !== 97 || bytes[2] !== 115 || bytes[3] !== 109) throw new AssetFailure("asset_format");
  const hash = await crypto.subtle.digest("SHA-256", bytes);
  if ([...new Uint8Array(hash)].map(byte => byte.toString(16).padStart(2, "0")).join("") !== expected.toLowerCase()) throw new AssetFailure("integrity_mismatch");
}
async function probeWorker(signal: AbortSignal): Promise<void> {
  let worker: Worker | undefined;
  try {
    await abortable(signal, () => new Promise<void>((resolve, reject) => {
      worker = new Worker(new URL("./setup.worker.ts", import.meta.url), { type: "module" });
      worker.onmessage = event => event.data === "z-stack/setup-ready" ? resolve() : reject(new Error());
      worker.onerror = () => reject(new Error());
      worker.onmessageerror = () => reject(new Error());
    }));
  } finally { worker?.terminate(); }
}

/** Only fixed codes/messages leave this function; URLs, provider responses and causes do not. */
export async function checkWalletSetup(options: SetupCheckOptions = {}): Promise<SetupReport> {
  const checks: SetupCheck[] = [];
  const add = (code: SetupCheckCode, status: SetupCheck["status"]) => checks.push({ code, status, message: messages[code] });
  if (typeof window === "undefined") { add("browser", "fail"); return { ok: false, checks }; }
  add("browser", "pass");
  add("secure_context", globalThis.isSecureContext === true ? "pass" : "fail");
  add("webassembly", typeof WebAssembly !== "undefined" ? "pass" : "fail");
  add("webcrypto", typeof crypto !== "undefined" && !!crypto.subtle ? "pass" : "fail");
  add("storage", typeof indexedDB !== "undefined" ? "pass" : "warning");
  add("isolation", globalThis.crossOriginIsolated === true && typeof SharedArrayBuffer !== "undefined" ? "pass" : "warning");
  add("wallet_owner", browserWalletActive() ? "warning" : "pass");
  const controller = new AbortController();
  const cancel = () => controller.abort();
  options.signal?.addEventListener("abort", cancel, { once: true });
  if (options.signal?.aborted) cancel();
  const duration = Number.isFinite(options.timeoutMs) ? Math.max(1, Math.min(60_000, options.timeoutMs!)) : 10_000;
  const timer = setTimeout(cancel, duration);
  try {
    if (options.worker) {
      try { await probeWorker(controller.signal); add("module_worker", "pass"); }
      catch { add("module_worker", "fail"); }
    } else add("module_worker", "skipped");
    if (options.assets) {
      try { await abortable(controller.signal, () => probeAssets(options.assets!, controller.signal)); add("assets", "pass"); }
      catch (error) { add(error instanceof AssetFailure ? error.code : "asset_unavailable", "fail"); }
    } else add("assets", "skipped");
    if (options.server) {
      try {
        const capabilities = await abortable(controller.signal, () => options.server!(controller.signal));
        add("server", capabilities.reachable === true ? "pass" : "fail");
        add("shared_memos", capabilities.reachable === true && capabilities.sharedMemos === true ? "pass" : "warning");
      } catch { add("server", "fail"); }
    } else add("server", "skipped");
    if (options.signal?.aborted) add("cancelled", "fail");
    return { ok: checks.every(check => check.status !== "fail"), checks };
  } finally { clearTimeout(timer); options.signal?.removeEventListener("abort", cancel); }
}
