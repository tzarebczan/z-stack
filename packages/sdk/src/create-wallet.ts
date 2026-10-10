/**
 * One call from "nothing" to a wallet: load the WASM engine, pick a transport
 * for the light server, and return the wallet client.
 */

import { WalletError, type UnlockPolicy, type WalletSnapshot } from "@z-stack/core";
import type { CreationPreparation, EngineClient } from "./engine";
import { grpcWebTransport, httpLwdTransport, isLoopbackUrl, looksLikeLwdPipe, type BlockTransport } from "./lwd";
import { initialize, wasmRuntime, type Network, type SdkInitOptions, type WasmRuntime } from "./runtime";
import { observeEngineProgress, type EngineLoadProgress } from "./engine-progress";
import { cancelWasmSync, createWasmClient, forgetWasmWallet, peekWasmWallet, type WasmClient, type WasmProgress } from "./wasm-client";
import { useWalletStorage, abortable, walletStorageAvailable } from "./wallet-storage";
import { type WalletStorage } from "./storage";
import { claimWalletOwner, releaseWalletOwner } from "./wallet-owner";

export interface WalletUnlockRequest {
  network: Network;
  /** Use locally to select the matching secret. Never send to a provider implicitly. */
  ufvk: string;
  signal: AbortSignal;
}
export interface WalletUnlocker {
  unlock(request: WalletUnlockRequest): Promise<string>;
}
export type WalletImportOptions = Pick<NonNullable<Parameters<EngineClient["restoreUfvk"]>[3]>, "replace" | "signal" | "assertCurrent" | "beforeCommit"> & {
  birthday?: Parameters<EngineClient["restore"]>[2];
};
export interface WalletCreation { wallet: WalletSnapshot; recoveryPhrase: string }
export type WalletCreationPreparation = CreationPreparation;
export type WalletCreateOptions = {
  birthday?: Parameters<EngineClient["create"]>[1];
  replace?: boolean;
  /** Optional recovery confirmation/backup before any wallet generation or snapshot is saved.
   * Honor the signal; external writes and their rollback belong to your app. */
  beforeCommit?: (creation: WalletCreationPreparation) => void | Promise<void>;
};

export type WalletOptions = {
  /** Local storage only. Remote encrypted backups are a separate optional service. */
  storage?: WalletStorage;
  /** Optional application-owned unlock method. No account/backend is required. */
  unlocker?: WalletUnlocker;
  network: Network;
  /**
   * The light server: a gRPC-Web URL (e.g. `https://zcash-testnet.chainsafe.dev` for testnet), a
   * local `z-wallet pipe` URL (`http://127.0.0.1:1239`), or any
   * {@link BlockTransport}. Keys never leave the browser either way.
   */
  server: string | BlockTransport;
  /**
   * Look up the wallet's transparent address on the server, so deposits to it
   * show up and can be shielded. The server learns the address. Default: on
   * for local servers, off for remote ones.
   */
  transparent?: boolean;
  /**
   * Shield eligible transparent funds after sync when unlocked. Explicit opt-in;
   * default false. With each-spend, shielding consumes the unlock; unlock again
   * before sending. No unlocker is called automatically.
   */
  autoShield?: boolean;
  /**
   * Fetch full wallet transactions for memos during sync (default on-demand). The
   * server learns their transaction IDs. With on-demand, only fetchMemos()
   * requests them; local history, compact scanning and broadcasts still work.
   */
  memoFetch?: "auto" | "on-demand" | "shared";
  /**
   * For a URL server, explicitly enable its optional /zstack/memos extension.
   * Default false. memoFetch: "shared" selects privacy policy; it does not
   * claim that an ordinary gRPC-Web server supports shared memo ranges.
   * Custom BlockTransport objects declare support with their sharedMemos method.
   */
  sharedMemos?: boolean;
  /** Scan transparent inputs/outputs locally; requires protocol >= 0.5. Default off. */
  transparentScan?: "compact" | "off";
  /**
   * Allow a scan of more than 150,000 blocks in one go (an old birthday, or a
   * wallet reopened after months). It downloads every compact block in the
   * range from the server. Default: on for a local pipe, off for remote
   * servers, where such a restore fails with `deep_sync_rejected`.
   */
  deepSync?: boolean;
  /** Retry transient sync outages for this many milliseconds after the first failure. Default 90,000; zero disables retry. Transport request timeouts are separate. */
  lightServerGraceMs?: number;
  /**
   * `"each-spend"` (default): unlock before every spend. `"session"` keeps
   * the seed in memory until lock/close; reload always locks. Neither writes
   * plaintext keys to browser storage.
   */
  unlockPolicy?: Exclude<UnlockPolicy, "always">;
  /** Detailed scan progress (percent, ETA, stage), in addition to `wallet.on("sync")`. */
  onProgress?: (progress: WasmProgress) => void;
  /** Local WASM download/verification/startup, including the background scanner. Released on close or failed creation. */
  onLoadProgress?: (progress: EngineLoadProgress) => void;
  /**
   * Keep up with new blocks: check the tip every `intervalMs` (default 20 s)
   * while the page is visible, sync when behind, and catch up when the tab
   * comes back. Same as calling `wallet.startAutoSync()`. Default off.
   */
  autoSync?: boolean | { intervalMs?: number };
  /** Prepare the proving key after a funded sync, never during initial loading. Default true. */
  prewarmProvingKey?: boolean;
} & Pick<SdkInitOptions, "threads" | "preferMulticore" | "regtestNu63Height" | "regtestNu7Height" | "wasmBasePath">;

export type Wallet = Omit<WasmClient, "baseUrl" | "health" | "probeSetup" | "saveSetup" | "dispose" | "create" | "restore" | "restoreUfvk" | "restoreHardware" | "attachSeed" | "setUnlockPolicy" | "unlockPolicy" | "forgetSavedWallet"> & {
  /** The recovery phrase is separate from state/events and returned only here. */
  create(options?: WalletCreateOptions): Promise<WalletCreation>;
  restore(mnemonic: string, options?: { birthday?: Parameters<EngineClient["restore"]>[2]; replace?: boolean }): Promise<WalletSnapshot>;
  restoreUfvk(ufvk: string, options?: WalletImportOptions): Promise<WalletSnapshot>;
  restoreHardware(account: Parameters<EngineClient["restoreHardware"]>[0], options?: { birthday?: Parameters<EngineClient["restoreHardware"]>[2]; replace?: boolean }): Promise<WalletSnapshot>;
  /** Call from the user's action. A promise may start a prepared passkey ceremony in that same click. */
  unlock(secret?: string | Promise<string>): Promise<WalletSnapshot>;
  setUnlockPolicy(policy: Exclude<UnlockPolicy, "always">): Promise<WalletSnapshot | void>;
  unlockPolicy(): Exclude<UnlockPolicy, "always">;
  readonly network: Network;
  /**
   * How the engine runs here: multi-threaded (cross-origin isolated) or
   * single-threaded. Live: the multicore scan worker comes up in the
   * background. Subscribe to `on("runtime", handler)` for readiness and fallback;
   * that subscription immediately supplies the current runtime, without polling.
   */
  readonly runtime: WasmRuntime;
  /** The wallet saved on this device, or null. Reads the configured local store only (no network). */
  load(): Promise<WalletSnapshot | null>;
  /**
   * Delete this device's wallet: snapshot, cached seed and passphrase vault.
   * The passkey record stays unless `{ passkey: true }`. Auto-sync pauses
   * during deletion and resumes if still enabled; the client remains usable.
   * `{ pending: "reject" }` checks durable outgoing reservations under the spend
   * lock and deletes only that inspected revision. Rejects `forget_pending` or
   * `wallet_changed` rather than deleting newer state. Default is explicit deletion.
   */
  forget(opts?: { passkey?: boolean; pending?: "reject" }): Promise<void>;
  /** Stop a running sync (it resolves with the progress so far). */
  cancelSync(): void;
  /** Follow new blocks (see the `autoSync` option in {@link WalletOptions}). Idempotent. */
  startAutoSync(intervalMs?: number): void;
  stopAutoSync(): void;
  /**
   * Lock, cancel work and release the owner. Saved data stays. Await before
   * opening another client. A rollback storage error rejects after cleanup;
   * inspect the saved wallet on reopening before attempting another spend.
   */
  close(): Promise<void>;
};

const AUTO_SYNC_MS = 20_000;

/** Pick the transport for a light-server URL. */
export function lightServer(url: string, opts: { network: Network; transparent?: boolean; sharedMemos?: boolean }): BlockTransport {
  const trimmed = url.trim();
  if (looksLikeLwdPipe(trimmed)) {
    // Port 1239 is the pipe, including on a public host. Transparent lookups
    // stay on loopback unless the caller opts in.
    return httpLwdTransport(trimmed, opts.network, undefined, undefined, undefined, {
      transparent: opts.transparent ?? isLoopbackUrl(trimmed),
    });
  }
  return grpcWebTransport(trimmed, { transparent: opts.transparent ?? isLoopbackUrl(trimmed), sharedMemos: opts.sharedMemos });
}

/**
 * Create the in-browser wallet. Call once per page: the engine keeps one
 * active wallet per JavaScript realm. The returned client owns the engine;
 * its configured local store owns saved state.
 *
 * ```ts
 * const wallet = await createWallet({ network: "testnet", server: "https://zcash-testnet.chainsafe.dev" });
 * const saved = await wallet.load();
 * if (!saved) await wallet.restore(mnemonic, { birthday: "2026-09-01" });
 * wallet.on("balance", (b) => render(b));
 * await wallet.sync();
 * ```
 */
export async function createWallet(opts: WalletOptions): Promise<Wallet> {
  if (!["mainnet", "testnet", "regtest"].includes(opts.network)) throw new WalletError("invalid_network", "Choose mainnet, testnet or regtest.");
  if (opts.unlockPolicy !== undefined && !["session", "each-spend"].includes(opts.unlockPolicy)) throw new WalletError("unknown", "Invalid browser unlock policy.");
  const claimed = claimWalletOwner();
  if (!claimed) throw new WalletError("owner_conflict", "Only one browser wallet client can own the engine. Close it before creating another.");
  const lease = claimed;
  let offLoadProgress = () => {};
  let createdClient: WasmClient | undefined;
  try {
    if (opts.lightServerGraceMs !== undefined && (!Number.isSafeInteger(opts.lightServerGraceMs) || opts.lightServerGraceMs < 0 || opts.lightServerGraceMs > 2_147_483_647)) {
      throw new WalletError("unknown", "lightServerGraceMs must be a nonnegative integer below 2,147,483,648.");
    }
    useWalletStorage(opts.storage);
    const onLoadProgress = opts.onLoadProgress;
    if (onLoadProgress) offLoadProgress = observeEngineProgress(onLoadProgress);
    if (!walletStorageAvailable()) throw new WalletError("wallet_db", "Local storage is unavailable. Supply a WalletStorage adapter before creating a browser wallet.");
    const runtime = await initialize({
      network: opts.network,
      ...(opts.threads !== undefined ? { threads: opts.threads } : {}),
      ...(opts.preferMulticore !== undefined ? { preferMulticore: opts.preferMulticore } : {}),
      // Wait for a funded wallet and its scan worker. An eager init build can
      // otherwise build the same key in both ST and MT workers during catch-up.
      prewarmProvingKey: false,
      ...(opts.regtestNu63Height !== undefined ? { regtestNu63Height: opts.regtestNu63Height } : {}),
      ...(opts.regtestNu7Height !== undefined ? { regtestNu7Height: opts.regtestNu7Height } : {}),
      ...(opts.wasmBasePath !== undefined ? { wasmBasePath: opts.wasmBasePath } : {}),
    });
    const transport =
      typeof opts.server === "string"
        ? lightServer(opts.server, { network: opts.network, transparent: opts.transparent, sharedMemos: opts.sharedMemos })
        : opts.server;
    const lightUrl = typeof opts.server === "string" ? opts.server : undefined;
    // `close` drops this. The wasm client keeps the function it was given, so
    // the in-flight sync must call through a binding we can clear.
    let reportProgress = opts.onProgress;
    const client = createWasmClient(
      {
        requireExplicitReplacement: true,
        network: opts.network,
        transport,
        requirePersistence: true,
        ...(lightUrl ? { lightUrl } : {}),
        allowDeepSync: opts.deepSync ?? transport.kind === "lwd-pipe",
        ...(opts.lightServerGraceMs !== undefined ? { lightServerGraceMs: opts.lightServerGraceMs } : {}),
        ...(opts.autoShield !== undefined ? { autoShield: opts.autoShield } : {}),
        ...(opts.prewarmProvingKey !== undefined ? { prewarmProvingKey: opts.prewarmProvingKey } : {}),
        ...(opts.memoFetch !== undefined ? { memoFetch: opts.memoFetch } : {}),
        ...(opts.transparentScan !== undefined ? { transparentScan: opts.transparentScan } : {}),
        ...(opts.unlockPolicy ? { unlockPolicy: opts.unlockPolicy } : {}),
      },
      (progress) => reportProgress?.(progress),
    );
    createdClient = client;
    Object.defineProperty(client, "runtime", { get: () => wasmRuntime() ?? runtime, enumerable: true });

    let memoFetchMode = opts.memoFetch ?? "on-demand";
    let timer: ReturnType<typeof setInterval> | null = null;
    let autoSyncInterval: number | null = null;
    let forgetting = 0;
    let checking = false;
    let closed = false;
    let closing: Promise<void> | null = null;
    let unlockAbort = new AbortController();
    let keyEpoch = 0;
    function lock(): void { keyEpoch++; unlockAbort.abort(); unlockAbort = new AbortController(); client.lock(); }
    const visible = () => typeof document === "undefined" || document.visibilityState !== "hidden";
    async function followTip(): Promise<void> {
      if (closed || forgetting || checking || !visible()) return;
      const epoch = keyEpoch;
      checking = true;
      try {
        const saved = await peekWasmWallet();
        if (!saved || closed || forgetting || epoch !== keyEpoch) return;
        if (saved.network && saved.network !== opts.network) {
          throw new WalletError(
            "invalid_network",
            `saved wallet is ${saved.network}; this client is ${opts.network}`,
          );
        }
        const { behind } = await wallet.tip();
        if (closed || forgetting || epoch !== keyEpoch) return;
        const snapshot = await wallet.getWallet();
        if (closed || forgetting || epoch !== keyEpoch) return;
        // Manual memo batches cannot be advanced by ordinary sync. Only the
        // automatic policies count as background work at an unchanged tip.
        const pending = [snapshot.transparentScanStatus, memoFetchMode === "on-demand"
          ? undefined : snapshot.memoFetchStatus ?? snapshot.sharedMemoStatus]
          .some((status) => status === "scanning" || status === "unavailable");
        if (behind > 0 || pending) await wallet.sync();
      } catch {
        console.warn("auto-sync");
      } finally {
        checking = false;
      }
    }
    const onVisible = () => {
      if (visible()) void followTip();
    };
    function pauseAutoSync(): void {
      if (timer) clearInterval(timer);
      timer = null;
      if (typeof document !== "undefined") document.removeEventListener("visibilitychange", onVisible);
    }
    function resumeAutoSync(): void {
      if (closed || forgetting || autoSyncInterval === null) return;
      timer = setInterval(() => void followTip(), autoSyncInterval);
      if (typeof document !== "undefined") document.addEventListener("visibilitychange", onVisible);
    }
    function startAutoSync(intervalMs = AUTO_SYNC_MS): void {
      autoSyncInterval = Math.max(1_000, intervalMs);
      pauseAutoSync();
      resumeAutoSync();
    }
    function stopAutoSync(): void {
      autoSyncInterval = null;
      pauseAutoSync();
    }
    function close(): Promise<void> {
      if (closing) return closing;
      closed = true;
      lock();
      reportProgress = undefined;
      offLoadProgress();
      stopAutoSync();
      closing = client.dispose().finally(() => {
        if (releaseWalletOwner(lease)) useWalletStorage();
      });
      return closing;
    }
    if (opts.autoSync) startAutoSync(typeof opts.autoSync === "object" ? opts.autoSync.intervalMs : undefined);

    const createWalletOn = client.create.bind(client);
    const restoreWallet = client.restore.bind(client);
    const restoreViewingKey = client.restoreUfvk.bind(client);
    const restoreDevice = client.restoreHardware.bind(client);

    const surface = Object.fromEntries(Object.entries(client).filter(([key]) =>
      !["baseUrl", "health", "probeSetup", "saveSetup", "dispose", "loadSavedWallet", "attachSeed", "forgetSavedWallet"].includes(key)));
    const target = Object.assign(surface, {
      get runtime() { return wasmRuntime() ?? runtime; },
      network: opts.network,
      async create(options: WalletCreateOptions = {}) {
        lock();
        const created = await createWalletOn(opts.network, options.birthday, { replace: options.replace, beforeCommit: options.beforeCommit });
        const { mnemonic, ...snapshot } = created;
        if (!mnemonic) throw new WalletError("unknown", "Engine created a wallet without returning its recovery phrase.");
        return { wallet: snapshot, recoveryPhrase: mnemonic };
      },
      restore: (mnemonic: string, options: { birthday?: Parameters<EngineClient["restore"]>[2]; replace?: boolean } = {}) => {
        lock();
        return restoreWallet(mnemonic, opts.network, options.birthday, { replace: options.replace });
      },
      restoreUfvk: (ufvk: string, options: WalletImportOptions = {}) => {
        lock();
        return restoreViewingKey(ufvk, opts.network, options.birthday, options);
      },
      restoreHardware: (account: Parameters<EngineClient["restoreHardware"]>[0], options: { birthday?: Parameters<EngineClient["restoreHardware"]>[2]; replace?: boolean } = {}) => {
        lock();
        return restoreDevice(account, opts.network, options.birthday, options);
      },
      async unlock(secret?: string | Promise<string>) {
        const signal = unlockAbort.signal;
        const words = await abortable(signal, () => secret !== undefined ? Promise.resolve(secret) : (async () => {
          if (!opts.unlocker) throw new WalletError("seed_locked", "Supply a recovery phrase or configure an unlocker.");
          const snapshot = await client.getWallet();
          signal.throwIfAborted();
          if (!snapshot.ufvk) throw new WalletError("view_only", "The wallet has no viewing key to verify the secret.");
          return opts.unlocker.unlock({ network: opts.network, ufvk: snapshot.ufvk, signal });
        })());
        signal.throwIfAborted();
        const unlocked = await client.attachSeed(words, { signal });
        signal.throwIfAborted();
        keyEpoch++;
        return unlocked;
      },
      setUnlockPolicy(policy: "session" | "each-spend") {
        if (!["session", "each-spend"].includes(policy)) throw new WalletError("unknown", "Invalid browser unlock policy.");
        if (policy === "each-spend") lock();
        return client.setUnlockPolicy(policy);
      },
      lock,
      load: () => client.loadSavedWallet(),
      async forget(o?: { passkey?: boolean; pending?: "reject" }) {
        lock();
        forgetting++;
        pauseAutoSync();
        try {
          await client.forgetSavedWallet(o);
        } finally {
          forgetting--;
          // The client remains usable after deletion. Preserve its current
          // sync preference, including stop/start/close during the await.
          if (!forgetting) resumeAutoSync();
        }
      },
      setMemoFetch(mode: Parameters<WasmClient["setMemoFetch"]>[0]) {
        client.setMemoFetch(mode);
        memoFetchMode = mode;
      },
      cancelSync: () => cancelWasmSync(),
      startAutoSync,
      stopAutoSync,
      close,
    });
    Object.defineProperty(target, "runtime", { get: () => wasmRuntime() ?? runtime, enumerable: true });
    // Public calls normalize errors once, including raw engine/transport failures.
    const wallet = new Proxy(target, {
      get(object, key, receiver) {
        const value = Reflect.get(object, key, receiver);
        if (typeof value !== "function" || key === "close") return value;
        return (...args: unknown[]) => {
          if (closed) {
            const error = new WalletError("closed", "This wallet client is closed.");
            if (["on", "off", "lock", "unlockPolicy", "hasSpendingSeed", "setMemoFetch", "setTransparentScan", "setPrewarmProvingKey", "cancelSync", "startAutoSync", "stopAutoSync"].includes(String(key))) throw error;
            return Promise.reject(error);
          }
          try {
            const epoch = keyEpoch;
            const result = value.apply(object, args);
            if (!(result instanceof Promise)) return result;
            const normalized = result.catch(error => { throw WalletError.fromUnknown(error); });
            return ["send", "sendTransparent", "shield"].includes(String(key))
              ? normalized.finally(() => { if (epoch === keyEpoch && client.unlockPolicy() === "each-spend") lock(); }) : normalized;
          } catch (error) { throw WalletError.fromUnknown(error); }
        };
      },
    }) as unknown as Wallet;
    return wallet;
  } catch (error) {
    offLoadProgress();
    try { await createdClient?.dispose(); }
    catch { console.warn("wallet initialization cleanup failed"); }
    finally { if (releaseWalletOwner(lease)) useWalletStorage(); }
    throw WalletError.fromUnknown(error);
  }
}

/** Delete a local wallet without loading WASM. Use the owning client's forget() when it is open. */
export async function forgetWallet(options: { storage?: WalletStorage; passkey?: boolean } = {}): Promise<void> {
  const claimed = claimWalletOwner();
  if (!claimed) throw new WalletError("busy", "Use the open wallet client's forget() before closing it.");
  const lease = claimed;
  useWalletStorage(options.storage);
  try {
    if (!walletStorageAvailable()) throw new WalletError("wallet_db", "Local storage is unavailable. This device’s saved wallet has not been deleted.");
    await forgetWasmWallet({ passkey: options.passkey });
  }
  catch (error) { throw WalletError.fromUnknown(error); }
  finally { if (releaseWalletOwner(lease)) useWalletStorage(); }
}
