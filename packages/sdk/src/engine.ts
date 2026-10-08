/**
 * HTTP client for `z-wallet serve` / `z-desktop` loopback JSON.
 * Same method surface as {@link createWasmClient} ({@link EngineClient}).
 * Crypto stays in the engine. Pass `http://127.0.0.1:8787` (or a device
 * equivalent). Do not bind that API off localhost. Does **not** load WASM —
 * skip {@link initialize} unless you also need in-tab keys/scan.
 */

import {
  parseZip321,
  WalletError,
  type HistoryEntry,
  type HistoryQuery,
  type InspectedAddress,
  type UnlockPolicy,
  type WalletSnapshot,
} from "@z-stack/core";
import { balanceEvent, createEventBus, type EventBus, type WalletEventHandler, type WalletEventName } from "./events";
import type { HardwareAccount, HardwareSendOptions } from "./hardware";
import { explainBridgeAuthError, splitProxyAuth, transportFetchOptions } from "./lwd";

export type { BalanceEvent, SyncEvent, WalletEventHandler, WalletEventMap, WalletEventName } from "./events";

/** Snapshot after a send or shield, with the broadcast transaction's id. */
export type SpendResult = WalletSnapshot & { txid?: string; txids?: string[] };

/** Creation succeeds at durable commit. Optional browser backup has its own outcome. */
export type CreationResult = WalletSnapshot & { localSeedBackup?: "saved" | "not-saved" };
export type CreationPreparation = { wallet: WalletSnapshot; recoveryPhrase: string; signal: AbortSignal };

function spendResult(snapshot: WalletSnapshot & { txid?: string; txids?: string[] }): SpendResult {
  const txid = snapshot.txid ?? snapshot.txids?.find((id) => typeof id === "string" && id.length > 0);
  return txid ? { ...snapshot, txid } : snapshot;
}

export type FeeEstimate = {
  feeZat: number;
  feeZec: string;
};

export type MaxSend = {
  maxSendZat: number;
  maxSendZec: string;
  feeZat: number;
  feeZec: string;
};

export type EngineHealth = {
  ok: boolean;
  wallet: boolean;
  bind: string;
  mode?: string;
};

export type ChainTip = { tip: number; scanned: number; behind: number };

export type WaitOpts = { timeoutMs?: number; intervalMs?: number };

export type LightProbe = {
  ok: boolean;
  url: string;
  chain: string;
  tip?: number | null;
  tScan?: boolean;
  error?: string | null;
};

export type ValidatorProbe = {
  ok: boolean;
  url: string;
  chain: string;
  height?: number | null;
  subversion?: string | null;
  error?: string | null;
};

export type SetupProbe = {
  network: string;
  light: LightProbe;
  validator: ValidatorProbe;
  defaults: { light: string; validatorRpc: string; publicLight: string };
};

export type BirthdayInput = number | string;

export type CreateOpts = {
  /** Browser only. Confirm or encrypt recovery before the first snapshot commits.
   * Rejection/cancellation leaves no replacement wallet. App-owned writes need
   * their own rollback; use the supplied signal for cancellable preparation. */
  beforeCommit?: (creation: CreationPreparation) => void | Promise<void>;
  /** Browser only: explicitly consent to replacing a saved wallet. */
  replace?: boolean;
  network: string;
  birthday?: BirthdayInput;
  server?: string;
  validatorRpc?: string;
  /** Optional. Encrypts the mnemonic on this device (WASM) or seeds SeedAuth (native).
   * Browser creation still returns its phrase if this backup fails; check
   * CreationResult.localSeedBackup separately before claiming backup success. */
  passphrase?: string;
  /**
   * WASM only. Registers a passkey as soon as the mnemonic exists (same click).
   * Native engine ignores this (OS credential store). Failure must not wipe the wallet.
   */
  passkey?: boolean;
};

export type RestoreOpts = {
  /** Browser only: explicitly consent to replacing a saved wallet. */
  replace?: boolean;
  server?: string;
  validatorRpc?: string;
  /** Save this policy for the restored wallet. Omitted keeps the engine's existing default. */
  unlockPolicy?: UnlockPolicy;
  /** Optional. Encrypts the mnemonic on this device (WASM) or seeds SeedAuth (native). */
  passphrase?: string;
  /** WASM only. Same as {@link CreateOpts.passkey}. */
  passkey?: boolean;
  /** WASM restoreUfvk only. Cancels preparation and any uncommitted replacement transaction. */
  signal?: AbortSignal;
  /** WASM restoreUfvk only. Revalidate the caller before asynchronous work can publish state. */
  assertCurrent?: () => void;
  /**
   * WASM restoreUfvk only. Prepare dependent state before the first durable wallet save.
   * The snapshot and history are readable here. Do not start sync or publish UI state.
   * A rejected/cancelled restore leaves the prior saved wallet intact. Callers that
   * write their own storage here must roll back those exact writes on failure.
   */
  beforeCommit?: (snapshot: WalletSnapshot) => void | Promise<void>;
};
export type SendOptions = Partial<HardwareSendOptions>;

export type EngineClientOpts = {
  /** Bearer token for `z-wallet serve` / desktop bridge (401 without it on spend routes). */
  token?: string;
};

/**
 * Shared wallet façade. Two constructors, same methods:
 *
 * - `createEngineClient` / `createNativeClient` — native sqlite over loopback
 *   JSON (`z-desktop` / `z-wallet serve` on `:8787`).
 * - `createWasmClient` — in-tab `z-wasm` (scan, history, Orchard prove).
 *
 * History is always `HistoryEntry` (`v_transactions` columns). WASM `saveSetup`
 * throws (lab URLs live in the page, not the snapshot).
 *
 * Listen with `on("sync" | "balance")` / `off`. Do not call `initialize()` for
 * the native constructor.
 */
export type EngineClient = {
  baseUrl: string;
  on: <K extends WalletEventName>(event: K, handler: WalletEventHandler<K>) => () => void;
  off: <K extends WalletEventName>(event: K, handler: WalletEventHandler<K>) => void;
  health: () => Promise<EngineHealth>;
  getWallet: () => Promise<WalletSnapshot>;
  sync: () => Promise<WalletSnapshot>;
  /** Shield transparent funds at or above `threshold` zatoshis. Resolves after broadcast. */
  shield: (threshold?: number) => Promise<SpendResult>;
  /**
   * Send to a unified address or a ZIP-321 `zcash:` URI (multi-pay `address.N`
   * ok). `amountZec` is decimal ZEC. Shielded destinations only on WASM.
   * Resolves after broadcast with the new snapshot and the txid.
   */
  send: (to: string, amountZec: string, memo?: string, opts?: SendOptions) => Promise<SpendResult>;
  /** Explicit reviewed swap output. Bare network-correct P2PKH/P2SH only, shielded inputs.
   * No URI/memo/multipay; WASM software wallets only. Exposes recipient and amount. */
  /** Explicit public swap output from shielded notes. Optional maximum fee is an exact integer zatoshi string. */
  sendTransparent: (to: string, amountZec: string, options?: { maxFeeZat?: string; beforeBroadcast?: () => boolean }) => Promise<SpendResult>;
  estimateTransparentFee: (to: string, amountZec: string) => Promise<FeeEstimate>;
  /** Runtime support only; spending still requires the matching seed and spendable notes. */
  supportsTransparentSend: () => Promise<boolean>;
  /** ZIP-317 fee from engine `propose_transfer`. Not a TypeScript pad. */
  estimateFee: (to: string, amountZec?: string, memo?: string) => Promise<FeeEstimate>;
  /** Max shielded sendable from engine propose (own UA if `to` omitted). */
  maxSend: (to?: string) => Promise<MaxSend>;
  /** Receivers on a UA / t-addr / sapling string. */
  inspectAddress: (encoded: string) => Promise<InspectedAddress>;
  create: (
    network: string,
    birthday?: BirthdayInput,
    opts?: Omit<CreateOpts, "network" | "birthday">,
  ) => Promise<CreationResult>;
  restore: (
    mnemonic: string,
    network: string,
    birthday?: BirthdayInput,
    opts?: RestoreOpts,
  ) => Promise<WalletSnapshot>;
  restoreUfvk: (
    ufvk: string,
    network: string,
    birthday?: BirthdayInput,
    opts?: RestoreOpts,
  ) => Promise<WalletSnapshot>;
  /**
   * A hardware-wallet account (browser engine): watch with the device's
   * viewing key, send with `send(to, amount, memo, { signer })`. Get the
   * account from the hardware entry point's `ledgerAccount` or Keystone's `zcash-accounts` QR.
   */
  restoreHardware: (account: HardwareAccount, network: string, birthday?: BirthdayInput, opts?: { replace?: boolean }) => Promise<WalletSnapshot>;
  attachSeed: (mnemonic: string, opts?: { signal?: AbortSignal }) => Promise<WalletSnapshot>;
  setUnlockPolicy: (policy: UnlockPolicy) => Promise<WalletSnapshot | void>;
  unlockPolicy: () => UnlockPolicy;
  hasSpendingSeed: () => boolean;
  /** Probe Zaino (light) and Zakura (RPC) independently. */
  probeSetup: (network: string, server?: string, rpc?: string) => Promise<SetupProbe>;
  saveSetup: (opts: { network: string; server?: string; validatorRpc?: string }) => Promise<WalletSnapshot>;
  /**
   * Newest first. `status` and `txid` are applied before the limit on the native bridge.
   * WASM filters inside the 500-row cap.
   */
  history: (limit?: number, query?: HistoryQuery) => Promise<HistoryEntry[]>;
  /** One row, or null when the wallet has no such txid. */
  transaction: (txid: string) => Promise<HistoryEntry | null>;
  /** Unmined rows that have not expired (`status=pending`). */
  pending: (limit?: number) => Promise<HistoryEntry[]>;
  tip: () => Promise<ChainTip>;
  nextAddress: () => Promise<WalletSnapshot>;
  /**
   * Wipe notes, trees, history, and compact-block cache.
   * Keys, birthday, addresses, and seed stay. Call `sync` to rescan from birthday.
   */
  resetScan: () => Promise<WalletSnapshot>;
  /** Sync in a loop until `scanned >= tip` or timeout. */
  waitUntilCaughtUp: (opts?: WaitOpts) => Promise<ChainTip>;
};

function jsonHeaders(token?: string, extra?: HeadersInit): HeadersInit {
  const h: Record<string, string> = { "content-type": "application/json" };
  if (token) h.authorization = `Bearer ${token}`;
  return { ...h, ...(extra ?? {}) };
}

async function req<T>(base: string, path: string, init?: RequestInit, token?: string): Promise<T> {
  let res: Response;
  try {
    res = await fetch(`${base}${path}`, {
      ...init,
      ...transportFetchOptions,
      headers: jsonHeaders(token, init?.headers),
    });
  } catch (e) {
    throw WalletError.fromUnknown(e);
  }
  const data = (await res.json().catch(() => ({}))) as T & { error?: string };
  if (!res.ok) {
    throw WalletError.fromMessage(
      explainBridgeAuthError(res.status, data.error) || data.error || `${res.status} ${path}`,
    );
  }
  return data;
}

async function afterWallet(
  bus: EventBus,
  work: Promise<WalletSnapshot>,
  sync?: boolean,
): Promise<WalletSnapshot> {
  if (sync) bus.emit("sync", { stage: "connecting" });
  try {
    const w = await work;
    if (sync) {
      bus.emit("sync", {
        stage: "synced",
        scanned: w.scannedHeight,
        percent: 100,
      });
    }
    const bal = balanceEvent(w);
    if (bal) bus.emit("balance", bal);
    return w;
  } catch (e) {
    if (sync) {
      const err = WalletError.fromUnknown(e);
      bus.emit("sync", { stage: "error", message: err.message });
      throw err;
    }
    throw WalletError.fromUnknown(e);
  }
}

function birthdayJson(birthday: BirthdayInput | undefined): number | string | undefined {
  if (birthday == null || birthday === "") return undefined;
  if (typeof birthday === "number") return birthday;
  const s = birthday.trim();
  if (!s) return undefined;
  if (/^\d+$/.test(s)) return Number(s);
  // The bridge resolves dates against its live tip and actual network schedule.
  // A typical-tip estimate can omit receipts on a faster or lagging chain.
  return s;
}

/** Native loopback JSON client. Alias: `createNativeClient`. Does not load WASM. */
export function createEngineClient(baseUrl: string, opts?: EngineClientOpts): EngineClient {
  const { base, token } = splitProxyAuth(baseUrl, opts?.token);
  let policy: UnlockPolicy = "session";
  const bus = createEventBus();
  const call = <T>(path: string, init?: RequestInit) => req<T>(base, path, init, token);
  const snap = (path: string, init?: RequestInit, sync?: boolean) =>
    afterWallet(bus, call<WalletSnapshot>(path, init).then((wallet) => {
      if (wallet.unlockPolicy) policy = wallet.unlockPolicy;
      return wallet;
    }), sync);
  return {
    baseUrl: base,
    on: (event, handler) => bus.on(event, handler),
    off: (event, handler) => bus.off(event, handler),
    health: () => call<EngineHealth>("/health"),
    getWallet: () => call<WalletSnapshot>("/wallet"),
    sync: () => snap("/sync", { method: "POST", body: "{}" }, true),
    shield: async (threshold) =>
      spendResult(await snap("/shield", {
        method: "POST",
        body: JSON.stringify({ threshold }),
      })),
    send: (to, amountZec, memo, sendOpts) => {
      if (sendOpts?.signer) {
        return Promise.reject(new WalletError("hardware_unsupported", "hardware wallets need the browser engine (createWallet)"));
      }
      if (sendOpts?.signal || sendOpts?.beforeBroadcast || sendOpts?.onStage) {
        return Promise.reject(new WalletError("unknown", "Send review and cancellation hooks require the browser engine."));
      }
      const dest = to.trim();
      try {
        if (dest.toLowerCase().startsWith("zcash:")) {
          parseZip321(dest);
        }
      } catch (e) {
        return Promise.reject(WalletError.fromUnknown(e));
      }
      return snap("/send", {
        method: "POST",
        body: JSON.stringify({ to: dest, amountZec, memo }),
      }).then(spendResult);
    },
    supportsTransparentSend: async () => false,
    sendTransparent: async () => { throw new Error("transparent swap outputs require the WASM software wallet"); },
    estimateTransparentFee: async () => { throw new Error("transparent swap outputs require the WASM software wallet"); },
    estimateFee: async (to, amountZec, memo) => {
      const dest = to.trim();
      try {
        if (dest.toLowerCase().startsWith("zcash:")) {
          parseZip321(dest);
        }
      } catch (e) {
        throw WalletError.fromUnknown(e);
      }
      return call<FeeEstimate>("/send/estimate", {
        method: "POST",
        body: JSON.stringify({ to: dest, amountZec, memo }),
      });
    },
    maxSend: (to) =>
      call<MaxSend>("/send/max", {
        method: "POST",
        body: JSON.stringify({ to: to?.trim() || undefined }),
      }),
    inspectAddress: (encoded) =>
      call<InspectedAddress>("/address/inspect", {
        method: "POST",
        body: JSON.stringify({ address: encoded.trim() }),
      }),
    create: (network, birthday, createOpts) =>
      createOpts?.beforeCommit ? Promise.reject(new Error("creation preparation requires the browser engine")) : snap("/create", {
        method: "POST",
        body: JSON.stringify({
          network,
          birthday: birthdayJson(birthday),
          server: createOpts?.server,
          validatorRpc: createOpts?.validatorRpc,
          passphrase: createOpts?.passphrase?.trim() || undefined,
        }),
      }),
    restore: (mnemonic, network, birthday, restoreOpts) =>
      restoreOpts?.signal || restoreOpts?.assertCurrent || restoreOpts?.beforeCommit
        ? Promise.reject(new Error("guarded restore requires browser restoreUfvk"))
        : snap("/restore", {
        method: "POST",
        body: JSON.stringify({
          mnemonic,
          network,
          birthday: birthdayJson(birthday),
          unlockPolicy: restoreOpts?.unlockPolicy,
          server: restoreOpts?.server,
          validatorRpc: restoreOpts?.validatorRpc,
          passphrase: restoreOpts?.passphrase?.trim() || undefined,
        }),
      }),
    restoreHardware: () =>
      Promise.reject(new WalletError("hardware_unsupported", "hardware wallets need the browser engine (createWallet)")),
    restoreUfvk: (ufvk, network, birthday, restoreOpts) =>
      restoreOpts?.signal || restoreOpts?.assertCurrent || restoreOpts?.beforeCommit
        ? Promise.reject(new Error("guarded restore requires browser restoreUfvk"))
        : snap("/restore", {
        method: "POST",
        body: JSON.stringify({
          ufvk: ufvk.trim(),
          network,
          birthday: birthdayJson(birthday),
          unlockPolicy: restoreOpts?.unlockPolicy,
          server: restoreOpts?.server,
          validatorRpc: restoreOpts?.validatorRpc,
          passphrase: restoreOpts?.passphrase?.trim() || undefined,
        }),
      }),
    attachSeed: (mnemonic) =>
      snap("/attach-seed", {
        method: "POST",
        body: JSON.stringify({ mnemonic }),
      }),
    setUnlockPolicy: async (next) => {
      const wallet = await snap("/unlock-policy", {
        method: "POST",
        body: JSON.stringify({ policy: next }),
      });
      policy = wallet.unlockPolicy ?? next;
      return wallet;
    },
    unlockPolicy: () => policy,
    hasSpendingSeed: () => false,
    probeSetup: (network, server, rpc) => {
      const q = new URLSearchParams({ network });
      if (server?.trim()) q.set("server", server.trim());
      if (rpc?.trim()) q.set("rpc", rpc.trim());
      return call<SetupProbe>(`/setup/probe?${q}`);
    },
    saveSetup: (setup) =>
      snap("/setup", {
        method: "POST",
        body: JSON.stringify(setup),
      }),
    history: async (limit = 50, query) => {
      const q = new URLSearchParams();
      q.set("limit", String(query?.txid ? 1 : limit));
      if (query?.status) q.set("status", query.status);
      if (query?.txid?.trim()) q.set("txid", query.txid.trim());
      const r = await call<{ transactions: HistoryEntry[] }>(`/history?${q}`);
      return r.transactions ?? [];
    },
    transaction: async (txid) => {
      const id = txid.trim();
      if (!id) return null;
      try {
        return await call<HistoryEntry>(`/tx?txid=${encodeURIComponent(id)}`);
      } catch (e) {
        const err = WalletError.fromUnknown(e);
        if (err.message.toLowerCase().includes("not found")) return null;
        throw err;
      }
    },
    pending: (limit = 50) => {
      const q = new URLSearchParams({ limit: String(limit), status: "pending" });
      return call<{ transactions: HistoryEntry[] }>(`/history?${q}`).then((r) => r.transactions ?? []);
    },
    tip: () => call<ChainTip>("/tip"),
    nextAddress: () => snap("/address/next", { method: "POST", body: "{}" }),
    resetScan: () => snap("/scan/reset", { method: "POST", body: "{}" }),
    waitUntilCaughtUp: async (wait) => {
      const timeoutMs = wait?.timeoutMs ?? 120_000;
      const intervalMs = wait?.intervalMs ?? 1_000;
      const start = Date.now();
      let last: ChainTip | null = null;
      while (Date.now() - start < timeoutMs) {
        await snap("/sync", { method: "POST", body: "{}" }, true);
        last = await call<ChainTip>("/tip");
        if (last.behind === 0) return last;
        await new Promise((r) => setTimeout(r, intervalMs));
      }
      throw WalletError.fromMessage(
        `still behind tip=${last?.tip} scanned=${last?.scanned} after ${timeoutMs}ms`,
      );
    },
  };
}

export async function probeEngine(baseUrl: string, opts?: EngineClientOpts): Promise<EngineHealth | null> {
  try {
    return await createEngineClient(baseUrl, opts).health();
  } catch {
    return null;
  }
}
