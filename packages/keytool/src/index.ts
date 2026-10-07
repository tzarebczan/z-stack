/** Seed-touching utilities — keep this package small and isolated. */

export function clearBytes(buf: Uint8Array): void {
  buf.fill(0);
}
