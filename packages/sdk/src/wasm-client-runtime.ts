import {
  HASH_KEEP,
  MAX_SYNC_BLOCKS,
  parseBirthdayInput,
  validateBirthdayInput,
  WalletError,
  type InspectedAddress,
  type UnlockPolicy,
  type WalletSnapshot
} from "@z-stack/core";
import { coalescedTask } from "./coalesced-task";
import type { BirthdayInput, EngineClient } from "./engine";
import type { HardwareStatics } from "./hardware-ops";
import type { BlockTransport } from "./lwd";
import { isBroadcastRejection, isDuplicateBroadcast } from "./lwd";
import {
  forgetScanWorkerWallet,
  scanWorkerRuntime,
  type ScanSession
} from "./scan-host";
import { persistEncryptedSeed } from "./seed-vault";
import {
  abortSnapshotWrites,
  clearSavedWallet,
  peekSavedSnapshot,
  type SnapshotReplacement,
} from "./snapshot-storage";
import { invalidateVaultOperations } from "./vault-operation";
import { runWalletForget, type WalletOperation } from "./wallet-lifecycle";
import { readWalletGeneration, walletStorageAvailable, type WalletGeneration } from "./wallet-storage";
import { runtimeState } from './wasm-client-coordination';
import { peekSnapshotBytes } from './wasm-client-preview';
export { runtimeState } from './wasm-client-coordination';
export { peekSnapshotBytes } from './wasm-client-preview';
export * from './wasm-client-workers';

export type Network = "mainnet" | "testnet" | "regtest";

export function radioConstrained(): boolean {
  if (typeof navigator === "undefined") return false;
  const c = (
    navigator as Navigator & {
      connection?: { saveData?: boolean; effectiveType?: string };
    }
  ).connection;
  if (!c) return false;
  if (c.saveData) return true;
  return c.effectiveType === "slow-2g" || c.effectiveType === "2g" || c.effectiveType === "3g";
}

/** Session-only spending seed (never written to IndexedDB). */
export function setWasmSpendingSeed(mnemonic: string): void {
  runtimeState.moduleSpendingSeed = mnemonic.trim() || null;
  saveSessionSeed(runtimeState.moduleSpendingSeed);
}

export const META_KEY = "meta";

export const MAX_GAP = MAX_SYNC_BLOCKS;

export const IDB_READ_MS = 15_000;

export const WORKER_START_MS = 25_000;

export const HYDRATE_MS = 180_000;

/** Old plaintext key: remove it on open, lock and close; never read or write a seed. */
export const SESSION_SEED_KEY = "z-stack.wasm.session-seed";

/** Leaves at least 30 of a transaction's 40 expiry blocks for proving and mining. */
export const SPEND_MAX_LAG_BLOCKS = 10;

/**
 * The node's explicit answer to a broadcast, or null when the outcome is
 * unknown (network error, timeout, cut stream, anything but a node reply).
 * A duplicate (already in the mempool or mined) means the transaction is out;
 * any other refusal (a conflicting spend, an invalid transaction) means it
 * never will be.
 */
export function submitVerdict(error: unknown): "accepted" | "rejected" | null {
  if (!isBroadcastRejection(error)) return null;
  return isDuplicateBroadcast(error.reason) ? "accepted" : "rejected";
}

/** True when a broadcast error says this transaction is already in the mempool or chain. */
export function isDuplicateBroadcastError(error: unknown): boolean {
  if (isBroadcastRejection(error)) return isDuplicateBroadcast(error.reason);
  const message = error instanceof Error ? error.message : String(error);
  const embedded = /rejected \(-?\d+\): ([\s\S]*)$/i.exec(message);
  return isDuplicateBroadcast((embedded?.[1] ?? message).trim());
}

export function withOriginSpendLock<T>(run: () => Promise<T>): Promise<T> {
  const runExclusive = (): Promise<T> => {
    if (runtimeState.originSpend) return Promise.reject(new WalletError("busy", "a wallet spend is already in progress"));
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    runtimeState.originSpend = gate;
    return Promise.resolve()
      .then(run)
      .finally(() => {
        if (runtimeState.originSpend === gate) runtimeState.originSpend = null;
        release();
      });
  };
  const locks = (globalThis as { navigator?: { locks?: LockManager } }).navigator?.locks;
  if (typeof locks?.request !== "function") return runExclusive();
  return locks.request("z-stack-wallet-spend", { ifAvailable: true }, (lock) => {
    if (!lock) throw new WalletError("busy", "a wallet spend is already in progress in another tab");
    return runExclusive();
  }) as Promise<T>;
}

/** The engine steps back 10, 20, 40… blocks per reorg restart; consensus forks are at most 100 deep. */
export const MAX_REORG_RESTARTS = 12;

/** Engine `rescan from` only. A birthday fail-close must not start a 100k catch-up. */
export function reorgRestartFrom(fromHeight: number, rescanFrom: number, birthday: number): number {
  const drop = Math.max(0, fromHeight - rescanFrom);
  if (drop > HASH_KEEP && rescanFrom <= birthday) {
    throw new Error(
      `scan rewinded to ${rescanFrom} from ${fromHeight}; IndexedDB snapshot was not overwritten`,
    );
  }
  return rescanFrom;
}

export function withTimeout<T>(p: Promise<T>, ms: number, message: string): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(message)), ms);
    p.then(
      (v) => {
        clearTimeout(timer);
        resolve(v);
      },
      (e) => {
        clearTimeout(timer);
        reject(e);
      },
    );
  });
}

export function saveSessionSeed(_mnemonic: string | null): void {
  // Purge old plaintext records. Secrets are held in memory only, regardless
  // of policy. Reload always requires an application-supplied unlock method.
  try {
    if (typeof sessionStorage === "undefined") return;
    sessionStorage.removeItem(SESSION_SEED_KEY);
  } catch {
    /* private mode */
  }
}

export function loadSessionSeed(): string | null {
  saveSessionSeed(null);
  return null;
}

export const saveSnapshot = coalescedTask();

// Weak references let a device-wide forget release in-process Rust wallets too,
// without keeping every previously created EngineClient alive indefinitely.
export const localWalletSessions = new Set<WeakRef<ScanSession>>();

export function bindPersistOnHide(): void {
  if (runtimeState.hideBound || typeof document === "undefined") return;
  runtimeState.hideBound = true;
  const flush = () => {
    void runtimeState.persistOnHide?.().catch((_e) => console.warn("snapshot save on hide failed"));
  };
  const page = document, host = window;
  const onHidden = () => { if (page.visibilityState === "hidden") flush(); };
  page.addEventListener("visibilitychange", onHidden);
  host.addEventListener("pagehide", flush);
  runtimeState.unbindHide = () => {
    page.removeEventListener?.("visibilitychange", onHidden);
    host.removeEventListener?.("pagehide", flush);
    runtimeState.hideBound = false;
    runtimeState.unbindHide = null;
  };
}

export type WasmWalletHandle = {
  free?: () => void;
  toSnapshot: () => Uint8Array;
  applyCompactBlocks: (blob: Uint8Array) => string;
  applyCompactBlocksSummary?: (blob: Uint8Array) => string;
  applyTreeState?: (json: string) => void;
  applySubtreeRoots?: (protocol: string, json: string) => number;
  treesReady?: () => boolean;
  sinsemillaLive?: () => boolean;
  subtreeRootCount?: (protocol: string) => number;
  subtreeRootsStart?: (protocol: string) => number;
  applyUtxos?: (json: string) => number;
  applyMempool?: (json: string) => number;
  history: (limit: number) => string;
  snapshotJson: (server: string) => string;
  scannedHeight: () => number;
  birthday: () => number;
  nextHeight: () => number;
  unifiedAddress: () => string;
  transparentAddress: () => string | undefined;
  nextUnifiedAddress: () => string;
  estimateFee?: (to: string, amountZec: string, memo?: string) => string;
  estimateTransparentFee?: (to: string, amountZec: string) => string;
  proveTransparentSend?: (mnemonic: string, to: string, amountZec: string, maxFeeZat?: string) => string;
  maxSend?: (to?: string) => string;
  proveSend: (mnemonic: string, to: string, amountZec: string, memo?: string) => string;
  proveShield: (mnemonic: string, thresholdZat: number) => string;
  enhanceRawTx?: (hex: string) => number;
  memoEnhancementTxids?: (limit: number) => string;
  rewindTo?: (height: number) => number;
  abandon?: (txid: string) => boolean;
  applyMinedTx?: (hex: string, time: number) => string;
  attachSeed?: (mnemonic: string) => void;
  recomputePools?: () => void;
  resetScan?: () => void;
  rescanFrom?: (birthday: number) => void;
};

export type Bindings = {
  generateMnemonic: () => string;
  accountFromMnemonic?: (mnemonic: string, network: string, accountIndex: number) => { free?: () => void };
  checkViewingKey?: (network: string, ufvk: string) => void;
  inspectAddress?: (encoded: string) => InspectedAddress;
  ufvkCovers?: (network: string, derived: string, stored: string) => void;
  warmOrchardProvingKey?: () => boolean;
  orchardProvingKeyReady?: () => boolean;
  WasmWallet: {
    create: (
      network: string,
      mnemonic: string,
      birthday: number,
      accountIndex: number,
    ) => WasmWalletHandle;
    fromSnapshot: (bytes: Uint8Array) => WasmWalletHandle;
    fromUfvk?: (network: string, ufvk: string, birthday: number, accountIndex: number) => WasmWalletHandle;
    fromHardware?: (
      network: string,
      ufvk: string,
      birthday: number,
      device: string,
      seedFingerprint: string,
      accountIndex: number,
    ) => WasmWalletHandle;
    proveError: () => string;
    shieldError: () => string;
    capabilities: () => string;
  } & HardwareStatics;
};

/** Stop network/scan work without invalidating a spend's mandatory rollback save. */
export function interruptWasmSync(): void {
  runtimeState.wasmSyncEpoch += 1;
  runtimeState.syncAbort.abort();
  runtimeState.syncAbort = new AbortController();
}

export function cancelWasmSync(): void {
  interruptWasmSync();
  runtimeState.persistGen += 1;
  abortSnapshotWrites();
}

/**
 * Reject a mistyped phrase or key before a restore claims the wallet slot. The
 * slot's storage generation advances first, so a phrase Rust rejected later
 * had already fenced out the existing wallet's saved snapshot.
 */
export function assertRecoveryPhrase(words: string, network: string): void {
  const derive = requireBindings().accountFromMnemonic;
  if (typeof derive !== "function") return;
  try {
    derive(words, network, 0).free?.();
  } catch (e) {
    throw new Error(`invalid recovery phrase: ${e instanceof Error ? e.message : String(e)}`);
  }
}

export function assertViewingKey(ufvk: string, network: string): void {
  const check = requireBindings().checkViewingKey;
  if (typeof check !== "function") return;
  try {
    check(network, ufvk);
  } catch (e) {
    throw new Error(`invalid viewing key: ${e instanceof Error ? e.message : String(e)}`);
  }
}

export function requireBindings(): Bindings {
  // Loaded by initialize() in index.ts.
  const g = globalThis as unknown as { __zStackWasm?: Bindings };
  if (!g.__zStackWasm) {
    throw new WalletError("not_initialized", "SDK not initialized — call initialize() first");
  }
  return g.__zStackWasm;
}

export function attachWasmBindings(mod: Bindings): void {
  (globalThis as unknown as { __zStackWasm?: Bindings }).__zStackWasm = mod;
}

export type PasskeyWrite = { credentialId: string; replaced?: import("@z-stack/passkey").PasskeyVaultRecord };

export function persistPasskeyIfBrowser(mnemonic: string, operation: WalletOperation): Promise<PasskeyWrite | undefined> {
  operation.assertReady();
  if (!walletStorageAvailable() || typeof PublicKeyCredential === "undefined") {
    return Promise.resolve(undefined);
  }
  return import("./passkey").then(async ({ tryReplacePasskeySeed }) => {
    operation.assertCurrent();
    const r = await tryReplacePasskeySeed(mnemonic, operation.signal, replacementBases.get(operation.signal));
    if (r.status === "error") {
      console.warn("passkey register failed; wallet continues:");
      return undefined;
    }
    if (r.status !== "ok") return undefined;
    const written = { credentialId: r.credentialId, ...(r.replaced ? { replaced: r.replaced } : {}) };
    try {
      operation.assertCurrent();
    } catch (e) {
      await undoPasskeyIfBrowser(written, operation);
      throw e;
    }
    return written;
  });
}

/** The new wallet was not saved. Put the previous passkey vault back. */
export async function undoPasskeyIfBrowser(written: PasskeyWrite | undefined, operation: WalletOperation): Promise<void> {
  if (!written) return;
  try {
    const { undoPasskeySeedRegistration } = await import("./passkey");
    // The replacement may already have claimed a new generation. The rollback
    // has to match the generation stored now, not the one captured at the start.
    await undoPasskeySeedRegistration(written, operation.signal, readWalletGeneration(operation.signal));
  } catch {
    console.warn("passkey rollback failed");
  }
}

export async function persistSeedIfBrowser(passphrase: string, mnemonic: string, operation: WalletOperation, generation: WalletGeneration): Promise<void> {
  operation.assertCurrent();
  if (!walletStorageAvailable()) return;
  await persistEncryptedSeed(passphrase, mnemonic, operation.signal, generation);
}

export async function peekWasmWallet(): Promise<WalletSnapshot | null> {
  const generation = runtimeState.persistGen;
  try {
    const preview = await withTimeout(peekSavedSnapshot(peekSnapshotBytes), IDB_READ_MS, "IndexedDB wallet read timed out");
    return generation === runtimeState.persistGen ? preview : null;
  } catch {
    console.warn("peekWasmWallet");
    return null;
  }
}

export async function resolveBirthday(
  raw: BirthdayInput | undefined,
  tip: number,
  fallback: number,
  network: import("@z-stack/core").BirthdayNetwork = "mainnet",
): Promise<number> {
  if (raw == null || raw === "") return fallback;
  validateBirthdayInput(raw);
  return typeof raw === "number" ? raw : parseBirthdayInput(raw, tip, network);
}

export type WasmClientOpts = {
  /** Public Wallet always requires checkpoints. Raw storage-free lab clients are ephemeral. */
  requirePersistence?: true;
  /** Public browser façade requires explicit replacement; raw engine tools manage their own slot. */
  requireExplicitReplacement?: boolean;
  network: Network;
  transport: BlockTransport;
  /** Compact-block light URL (Zaino / LWD). Drives batch size; t-scan stays loopback-only. */
  lightUrl?: string;
  allowDeepSync?: boolean;
  /** Opt-in shielding after sync while unlocked. Default false; consumes an each-spend unlock. */
  autoShield?: boolean;
  /** Build the proving key after a funded sync. False disables automatic background builds. Default true. */
  prewarmProvingKey?: boolean;
  /** Shared public ranges, or selective transaction lookups (auto/on-demand). Default on-demand. */
  memoFetch?: "auto" | "on-demand" | "shared";
  /** Scan transparent inputs/outputs locally; requires protocol >= 0.5. Default off. */
  transparentScan?: "compact" | "off";
  shieldThresholdZat?: number;
  unlockPolicy?: UnlockPolicy;
};

export { validateBirthdayInput };

export type WasmClient = EngineClient & {
  /** Clear scan state from an earlier birthday, without replacing keys or addresses.
   * Refuses unresolved outgoing payments. Persists the reset before returning;
   * call sync afterwards. Does not unlock spending or enable deepSync.
   */
  rescan(options: { birthday: BirthdayInput }): Promise<WalletSnapshot>;
  /** Internal owner cleanup. Public applications use Wallet.close(). */
  dispose(): Promise<void>;
  /** Internal saved-state hydration. Public applications use Wallet.load(). */
  loadSavedWallet(): Promise<WalletSnapshot | null>;
  lock(): void;
  /**
   * Fetch and save the next memo batch. Shared mode requests public ranges;
   * auto/on-demand modes send up to 500 wallet transaction IDs to the server.
   * Does not scan new shielded blocks or broadcast transactions.
   */
  fetchMemos(): Promise<WalletSnapshot>;
  /** Change automatic memo fetching for this client; already-sent requests cannot be undone. */
  setMemoFetch(mode: "auto" | "on-demand" | "shared"): void;
  setTransparentScan(mode: "compact" | "off"): void;
  /** Changes future background key builds; an already-running WASM build is not interruptible. */
  setPrewarmProvingKey(enabled: boolean): void;
};

export type WasmProgress = {
  stage: "connecting" | "downloading" | "scanning" | "enhancing" | "synced";
  scanned: number;
  downloaded?: number;
  tip: number;
  notesFound: number;
  spendsFound: number;
  percent: number;
  remainingSeconds?: number;
  remainingHuman?: string;
  blocksPerSecond?: number;
  heading?: string;
  message?: string;
  /** UI should refresh the plate from notes (do not wait for shard hashing). */
  paintWallet?: boolean;
  availableZat?: number;
  pendingZat?: number;
  availableZec?: string;
  pendingZec?: string;
  orchardAvailable?: number;
};

export const replacementBases = new WeakMap<AbortSignal, Promise<WalletGeneration>>();

export const replacementConsent = new WeakMap<AbortSignal, boolean>();

/** Shared by clients that can adopt the same worker session. */
export const pendingReplacementSaves = new Set<WalletGeneration>();

export const pendingReplacementCompletions = new Set<Promise<void>>();

export const retiredWorkerSessions = new WeakSet<ScanSession>();

/**
 * Wipe the snapshot, session seed, and `seed.enc`. Keeps `passkey.v1` so
 * Restore from passkey still works. The authenticator credential is never
 * deleted. Pass `{ passkey: true }` to also drop this origin's PRF copy.
 */
export function forgetWasmWallet(opts?: { passkey?: boolean }): Promise<void> {
  return runWalletForget(async () => {
    runtimeState.moduleSpendingSeed = null;
    saveSessionSeed(null);
    runtimeState.persistOnHide = null;
    cancelWasmSync();
    invalidateVaultOperations();
    for (const ref of localWalletSessions) await ref.deref()?.forget();
    localWalletSessions.clear();
    await forgetScanWorkerWallet();
    await clearSavedWallet(opts);
  });
}

export function wasmCapabilities(): {
  keys: boolean;
  sync: boolean;
  history: boolean;
  prove: boolean;
  transparentScan: boolean;
  transparentOutputs?: boolean;
  mempool?: boolean;
  multicore?: boolean;
  threads?: number;
  orchardCircuit?: boolean;
  simd?: boolean;
} {
  try {
    const caps = JSON.parse(requireBindings().WasmWallet.capabilities()) as ReturnType<typeof wasmCapabilities>;
    const wr = scanWorkerRuntime();
    if (wr) {
      caps.multicore = wr.mode === "multi-thread";
      caps.threads = wr.threads;
      caps.simd = wr.simd;
      caps.orchardCircuit = wr.orchardCircuit;
    }
    return caps;
  } catch {
    return { keys: true, sync: false, history: false, prove: false, transparentScan: false };
  }
}

export type ClaimedGeneration = SnapshotReplacement & { complete: () => void };
