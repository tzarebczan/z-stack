/** Base64url without padding: the encoding WebAuthn JSON uses for every binary field. */
export function toBase64Url(bytes: ArrayBuffer | ArrayBufferView): string {
  const view = bytes instanceof ArrayBuffer
    ? new Uint8Array(bytes)
    : new Uint8Array(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let s = "";
  for (let i = 0; i < view.length; i++) s += String.fromCharCode(view[i]!);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function fromBase64Url(text: string): Uint8Array<ArrayBuffer> {
  const b64 = text.replace(/-/g, "+").replace(/_/g, "/");
  const bin = atob(b64 + "=".repeat((4 - (b64.length % 4)) % 4));
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function utf8(text: string): Uint8Array<ArrayBuffer> {
  return new TextEncoder().encode(text) as Uint8Array<ArrayBuffer>;
}

export function randomBytes(n: number): Uint8Array<ArrayBuffer> {
  return crypto.getRandomValues(new Uint8Array(n));
}

/** Copy any BufferSource into a fresh Uint8Array (extension results may be views). */
export function bytesOf(source: BufferSource | undefined | null): Uint8Array<ArrayBuffer> | null {
  if (!source) return null;
  if (source instanceof ArrayBuffer) return new Uint8Array(source.slice(0));
  if (ArrayBuffer.isView(source)) {
    return new Uint8Array(source.buffer.slice(source.byteOffset, source.byteOffset + source.byteLength) as ArrayBuffer);
  }
  return null;
}

/** Best effort: overwrite key material once it is no longer needed. */
export function wipe(bytes: Uint8Array | null | undefined): void {
  bytes?.fill(0);
}
