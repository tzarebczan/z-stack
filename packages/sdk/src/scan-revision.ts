import type { ScanSession } from "./scan-host";

// New operations are conservatively treated as mutations. These reads must
// remain side-effect free with respect to the persisted Rust wallet state.
const READS = new Set<keyof ScanSession>([
  "snapshotJson", "toSnapshot", "persistenceSnapshot", "history", "historyQuery", "balanceWithConfirmations", "pendingRawTxs", "pendingSubmissions",
  "scannedHeight", "birthday", "nextHeight", "treesReady", "sinsemillaLive",
  "subtreeRootCounts", "subtreeRootsStart", "transparentAddress",
  "supportsPrivateTransparentCheckpoints", "privateTransparentSnapshot", "supportsTransparentBlocks", "supportsTransparentSend", "memoEnhancementTxids", "warmProvingKey",
]);

/** A revision is usable only when every possible mutation has settled. */
export function trackScanRevision(session: ScanSession): ScanSession {
  let revision = 0;
  let pending = 0;
  const idle = new Set<() => void>();
  let phaseGate: { promise: Promise<void>; release: () => void; reject: (error: unknown) => void } | null = null;
  let phaseTail = Promise.resolve();
  let phases = 0;
  const settled = () => { if (!pending) { for (const resolve of idle) resolve(); idle.clear(); } };
  const engine = session.privateTransparent;
  const candidateSnapshot = session.privateTransparentSnapshot;

  for (const name of Object.keys(session) as Array<keyof ScanSession>) {
    const operation = session[name];
    if (typeof operation !== "function" || READS.has(name)) continue;
    Object.defineProperty(session, name, { value: async (...args: unknown[]) => {
      if (name !== "forget") while (phaseGate) await phaseGate.promise;
      revision++;
      pending++;
      try {
        return await Reflect.apply(operation, session, args);
      } finally {
        // A failed call may have partially mutated state. Never mark it clean.
        revision++;
        pending--;
        settled();
      }
    } });
  }
  if (engine && candidateSnapshot) {
    session.commitPrivateTransparentCheckpoint = async (id, commit, assertCurrent, failed) => {
      assertCurrent();
      phases++;
      if (!phaseGate) {
        let release!: () => void, reject!: (error: unknown) => void;
        const promise = new Promise<void>((resolve, fail) => { release = resolve; reject = fail; });
        void promise.catch(() => {});
        phaseGate = { promise, release, reject };
      }
      const preceding = phaseTail;
      let releasePhase!: () => void;
      phaseTail = new Promise<void>(resolve => { releasePhase = resolve; });
      await preceding;
      try {
        if (pending) await new Promise<void>(resolve => { idle.add(resolve); });
        assertCurrent();
        revision++; pending++;
        try {
          const candidate = await candidateSnapshot.call(session, id);
          assertCurrent();
          let acknowledged = false;
          await commit(candidate, async () => {
            if (acknowledged) throw new Error("transparent_pir_checkpoint_already_acknowledged");
            await engine.call(session, "ack", new TextEncoder().encode(id));
            acknowledged = true;
          });
          if (!acknowledged) throw new Error("transparent_pir_checkpoint_not_acknowledged");
        } finally {
          revision++; pending--; settled();
        }
      } catch (error) {
        let disposition: "preserve" | "retire" = "retire";
        try { disposition = await failed(error, async () => { await engine.call(session, "cancel", new Uint8Array()); }); }
        catch { /* Keep the original failure if owner cleanup also fails. */ }
        if (disposition === "retire") phaseGate?.reject(error);
        throw error;
      } finally {
        phases--;
        if (!phases) { const gate = phaseGate; phaseGate = null; gate?.release(); }
        releasePhase();
      }
    };
  }
  session.persistenceRevision = () => pending || phases ? undefined : revision;
  return session;
}
