/** SHA-256 of a wasm blob, compared to the hex written by `scripts/build-wasm.mjs`. */

export async function sha256Hex(bytes: BufferSource): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

export async function assertSha256(bytes: BufferSource, expectedHex: string): Promise<void> {
  const got = await sha256Hex(bytes);
  const want = expectedHex.trim().toLowerCase();
  if (got !== want) {
    throw new Error(`z-wasm sha256 mismatch (expected ${want}, got ${got})`);
  }
}

/**
 * When `integrity.json` sits next to the wasm (produced by the build script),
 * hash the bytes and refuse a mismatch. Return those exact bytes for instantiation
 * so integrity checking does not cause a second download. A missing manifest
 * throws, unless the caller is a dev server loading the module-relative build
 * output that `pnpm build:wasm` has not produced yet.
 */
export function allowMissingBuiltWasm(explicitBase: boolean): boolean {
  if (explicitBase) return false;
  const dev = (import.meta as ImportMeta & { env?: { DEV?: boolean } }).env?.DEV;
  return dev === true;
}

export async function verifyWasmAt(
  wasmUrl: string,
  integrityUrl: string,
  options?: { allowMissing?: boolean; onProgress?: (progress: { phase: "download" | "verify"; loadedBytes: number; totalBytes?: number }) => void },
): Promise<ArrayBuffer | undefined> {
  const body = (await readManifest(integrityUrl)) as { sha256?: string } | undefined;
  // A missing file is only acceptable for the module-relative build output.
  // An explicit wasm base must fail closed.
  if (!body) {
    if (options?.allowMissing) return;
    throw new Error(`z-wasm integrity manifest missing at ${integrityUrl}`);
  }
  const expected = body.sha256;
  if (!expected) {
    throw new Error(`z-wasm integrity manifest at ${integrityUrl} has no sha256`);
  }
  const wasm = await fetch(wasmUrl);
  if (!wasm.ok) {
    throw new Error(`z-wasm missing at ${wasmUrl} (${wasm.status})`);
  }
  // Content-Length describes compressed transfers when Content-Encoding is set.
  // Cross-origin responses may expose Length but hide Encoding. Keep their
  // total unknown rather than turn compressed length into decoded percentage.
  const length = Number(wasm.headers.get("content-length"));
  const totalBytes = wasm.type !== "cors" && !wasm.headers.get("content-encoding") && Number.isSafeInteger(length) && length > 0 ? length : undefined;
  const notify = (phase: "download" | "verify", loadedBytes: number) => {
    try { options?.onProgress?.({ phase, loadedBytes, ...(totalBytes ? { totalBytes } : {}) }); }
    catch { console.warn("engine progress handler"); }
  };
  notify("download", 0);
  let bytes: ArrayBuffer;
  if (options?.onProgress && wasm.body) {
    const reader = wasm.body.getReader();
    const chunks: Uint8Array[] = [];
    let loaded = 0, lastReport = 0;
    try {
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        chunks.push(value); loaded += value.byteLength;
        const now = Date.now();
        if (now - lastReport >= 100) { notify("download", loaded); lastReport = now; }
      }
    } finally { reader.releaseLock(); }
    const joined = new Uint8Array(loaded);
    let offset = 0;
    for (const chunk of chunks) { joined.set(chunk, offset); offset += chunk.byteLength; }
    bytes = joined.buffer;
  } else bytes = await wasm.arrayBuffer();
  notify("verify", bytes.byteLength);
  await assertSha256(bytes, expected);
  return bytes;
}

/**
 * Bundlers inline the tiny manifest as a `data:` URL. Decode it here instead of
 * fetching it: a page CSP without `data:` in `connect-src` would block the
 * fetch and silently skip verification.
 */
async function readManifest(url: string): Promise<unknown> {
  const inline = /^data:application\/json(;base64)?,(.*)$/s.exec(url);
  if (inline) {
    const payload = inline[2] ?? "";
    return JSON.parse(inline[1] ? new TextDecoder().decode(Uint8Array.from(atob(payload), (c) => c.charCodeAt(0))) : decodeURIComponent(payload));
  }
  const meta = await fetch(url);
  if (meta.status === 404) return undefined;
  if (!meta.ok) {
    throw new Error(`z-wasm integrity manifest ${url} failed (${meta.status})`);
  }
  return meta.json();
}
