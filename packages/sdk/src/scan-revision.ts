import type { ScanSession } from "./scan-host";

// New operations are conservatively treated as mutations. These reads must
// remain side-effect free with respect to the persisted Rust wallet state.
const READS = new Set<keyof ScanSession>([
  "snapshotJson", "toSnapshot", "persistenceSnapshot", "history", "pendingRawTxs",
  "scannedHeight", "birthday", "nextHeight", "treesReady", "sinsemillaLive",
  "subtreeRootCounts", "subtreeRootsStart", "transparentAddress",
  "supportsTransparentBlocks", "supportsTransparentSend", "memoEnhancementTxids", "warmProvingKey",
]);

/** A revision is usable only when every possible mutation has settled. */
export function trackScanRevision(session: ScanSession): ScanSession {
  let revision = 0;
  let pending = 0;
  for (const name of Object.keys(session) as Array<keyof ScanSession>) {
    const operation = session[name];
    if (typeof operation !== "function" || READS.has(name)) continue;
    Object.defineProperty(session, name, { value: async (...args: unknown[]) => {
      revision++;
      pending++;
      try {
        return await Reflect.apply(operation, session, args);
      } finally {
        // A failed call may have partially mutated state. Never mark it clean.
        revision++;
        pending--;
      }
    } });
  }
  session.persistenceRevision = () => pending ? undefined : revision;
  return session;
}
