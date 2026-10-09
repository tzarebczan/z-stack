import { WalletError, type InspectedAddress, type UaReceiverSet } from "@z-stack/core";
import { canUseScanWorker, observeScanRuntime, scanWorkerFailed, scanWorkerPresent, scanWorkerRuntime } from "./scan-host";
import { reportEngineProgress } from "./engine-progress";

/**
 * WASM loading and key/address helpers. `index.ts` is the public surface;
 * `lab.ts` adds lab-only helpers.
 */

/** Keep lockstep with `packages/sdk/package.json`. */
export const SDK_VERSION = "0.1.0-alpha.7";

export const LOCAL_ZAINO_GRPC = "http://127.0.0.1:8137";
/** Development mainnet Zaino convention; configure your own endpoint explicitly. */
export const LOCAL_ZAINO_GRPC_MAINNET = "http://127.0.0.1:8138";
export const LOCAL_ZAINO_GRPC_WEB = "http://127.0.0.1:1234/zaino";
/** Development loopback gRPC-Web proxy (not a wallet bridge). */
export const LOCAL_GRPC_WEB = "http://127.0.0.1:1238";
/** `z-wallet pipe` — native gRPC fan-out, one HTTP stream (no Chrome 6-socket cap). */
export const LOCAL_LWD_PIPE = "http://127.0.0.1:1239";
export const REGTEST_ZAINO_GRPC = "http://127.0.0.1:28137";
export const REGTEST_ZEBRA_RPC = "http://127.0.0.1:29232";
/** Development testnet JSON-RPC convention; zcashd-style :18232 is also probed. */
export const LOCAL_ZAKURA_RPC_TESTNET = "http://127.0.0.1:28232";
export const LOCAL_ZAKURA_RPC_MAINNET = "http://127.0.0.1:8232";
export const TESTNET_PUBLIC_LWD = "https://testnet.zec.rocks:443";
export const MAINNET_PUBLIC_LWD = "https://zec.rocks:443";
/** Loopback `z-wallet serve` JSON API (never bind this off localhost). */
export const NATIVE_BRIDGE_URL = "http://127.0.0.1:8787";

export type LocalEndpoints = {
  network: Network;
  light: string;
  validatorRpc: string;
  publicLight: string;
};

/** Default loopback Zaino + Zakura URLs. They are configured separately. */
export function localEndpoints(network: Network): LocalEndpoints {
  switch (network) {
    case "regtest":
      return {
        network,
        light: REGTEST_ZAINO_GRPC,
        validatorRpc: REGTEST_ZEBRA_RPC,
        publicLight: REGTEST_ZAINO_GRPC,
      };
    case "testnet":
      return {
        network,
        light: LOCAL_ZAINO_GRPC,
        validatorRpc: LOCAL_ZAKURA_RPC_TESTNET,
        publicLight: TESTNET_PUBLIC_LWD,
      };
    default:
      return {
        network,
        light: LOCAL_ZAINO_GRPC_MAINNET,
        validatorRpc: LOCAL_ZAKURA_RPC_MAINNET,
        publicLight: MAINNET_PUBLIC_LWD,
      };
  }
}

export type Network = "mainnet" | "testnet" | "regtest";

export type AddressKind = "unified" | "sapling" | "p2pkh" | "p2sh" | "tex" | "sprout";

export interface SdkInitOptions {
  /**
   * Directory containing the ST `z_wasm_bg.wasm` and `integrity.json`, matching
   * this SDK's bindings. Relative to the document URL; shared by UI, scan and
   * proving workers. Custom artifacts use ST and do not select a sibling MT build.
   */
  wasmBasePath?: string;
  /** Raw wasm bytes (Node tests / workers). Skips fetch. */
  wasmModule?: BufferSource;
  lightwalletdUrl?: string;
  network?: Network;
  /** Use the atomics/rayon artifact when SharedArrayBuffer is available (default true). */
  preferMulticore?: boolean;
  /**
   * Rayon worker count for compact-block scan / orchard MSM.
   * Default: a bounded pool based on `navigator.hardwareConcurrency`.
   */
  threads?: number;
  /**
   * Spawn the ST prove worker at init so the first send does not pay worker+wasm load.
   * Default true in the browser. Ignored in Node / when `wasmModule` is set.
   */
  prewarmProveWorker?: boolean;
  /**
   * Build the Orchard proving key in the prove worker (or on this thread if no worker).
   * Default false so catch-up is not fighting a 20–60s key build. `createWasmClient` sync
   * warms it after the first successful scan; send still builds on demand.
   */
  prewarmProvingKey?: boolean;
  /**
   * Regtest only: the height at which the validator activates NU6.3
   * (Ironwood), e.g. 150 on `pnpm regtest:native:up`. Every WASM instance
   * (page, scan worker, prove worker) uses it. Default: the engine's
   * 1,000,000, matching the compose Zebra chain.
   */
  regtestNu63Height?: number;
  /** Regtest only: optional NU7 height, strictly after NU6.3. Unscheduled by default. */
  regtestNu7Height?: number;
}

/** Hard cap on `initThreadPool` size (wasm linear memory / stack). */
export const MAX_WASM_THREADS = 32;
/** Most default Rayon workers. wasm-bindgen-rayon clones the module per worker. */
export const DEFAULT_WASM_THREADS = 16;

export function hardwareThreadCount(): number {
  return typeof navigator !== "undefined" ? navigator.hardwareConcurrency || 4 : 4;
}

/**
 * Default Rayon pool for `logical` reported cores: one worker per physical
 * core. Browsers report logical cores, and SMT siblings slow the wasm scan: a
 * mainnet restore on a 16-core/32-thread desktop took 73 s with 8 workers,
 * 56 s with 16, 64 s with 24 and 80 s with 32, on quiet and busy ranges alike.
 * Above 16 logical cores assume two per physical core; below, keep at most 8.
 */
export function threadsForCores(logical: number): number {
  const n = Math.floor(logical);
  if (!(n >= 1)) return 1;
  return n > 16 ? Math.min(DEFAULT_WASM_THREADS, Math.floor(n / 2)) : Math.min(8, n);
}

export function defaultThreadCount(): number {
  return threadsForCores(hardwareThreadCount());
}

export type WasmRuntime = {
  mode: "single-thread" | "multi-thread";
  threads: number;
  sharedArrayBuffer: boolean;
  crossOriginIsolated: boolean;
  orchardCircuit: boolean;
  simd: boolean;
  /** Compact-block apply runs in a dedicated Worker (UI thread stays free). */
  scanWorker?: boolean;
  /** Background scanner startup; mode describes key bindings until it is ready. */
  scanner?: "starting" | "ready" | "main-thread";
};

export interface Account {
  network: Network;
  accountIndex: number;
  unifiedAddress: string;
  ufvk: string;
  transparentAddress: string | null;
}

export interface ParsedAddress {
  network: Network;
  kind: AddressKind;
}

type WasmBindings = {
  default: (module_or_path?: unknown) => Promise<unknown>;
  generateMnemonic: () => string;
  seedFingerprint?: (mnemonic: string) => string;
  accountFromMnemonic: (
    mnemonic: string,
    network: string,
    accountIndex: number,
  ) => {
    network: string;
    accountIndex: number;
    unifiedAddress: string;
    ufvk: string;
    transparentAddress?: string;
  };
  parseAddress: (encoded: string) => ParsedAddress;
  inspectAddress?: (encoded: string) => {
    network: string;
    kind: string;
    receivers?: string[];
    receiverSet?: string | null;
  };
  unifiedAddressForSet?: (ufvk: string, network: string, set: string) => string;
  cryptoSmoke: () => string;
  WasmEngine: new (
    network: string,
    serverUrl?: string,
  ) => {
    network(): string;
    serverUrl(): string;
  };
  WasmWallet: {
    create: (network: string, mnemonic: string, birthday: number, accountIndex: number) => unknown;
    fromSnapshot: (bytes: Uint8Array) => unknown;
    proveError: () => string;
    shieldError: () => string;
    capabilities: () => string;
  };
  initThreadPool?: (numThreads: number) => Promise<void>;
  threadCount?: () => number;
  warmOrchardProvingKey?: () => boolean;
  orchardProvingKeyReady?: () => boolean;
  setRegtestNu63Height?: (height: number) => void;
  setRegtestNu7Height?: (height: number) => void;
};

let wasm: WasmBindings | null = null;
let wasmPromise: Promise<WasmBindings> | null = null;
let initOpts: SdkInitOptions = {};
let runtime: WasmRuntime | null = null;
const runtimeListeners = new Set<(runtime: WasmRuntime) => void>();
export function observeWasmRuntime(handler: (runtime: WasmRuntime) => void): () => void {
  runtimeListeners.add(handler);
  return () => { runtimeListeners.delete(handler); };
}
function runtimeChanged(): void {
  const current = wasmRuntime();
  if (!current) return;
  for (const handler of runtimeListeners) {
    try { handler({ ...current }); } catch { console.warn("runtime event handler"); }
  }
}
observeScanRuntime(runtimeChanged);

function sabAvailable(): boolean {
  return (
    typeof SharedArrayBuffer !== "undefined" &&
    typeof crossOriginIsolated !== "undefined" &&
    crossOriginIsolated
  );
}

function resolveThreadCount(options: SdkInitOptions = initOpts): number {
  if (options.threads && options.threads > 0) {
    return Math.max(1, Math.min(MAX_WASM_THREADS, Math.floor(options.threads)));
  }
  return defaultThreadCount();
}

async function loadSingleThread(): Promise<WasmBindings> {
  const mod = (await import("./generated/z_wasm.js")) as WasmBindings;
  let source: unknown = initOpts.wasmModule;
  if (!source) {
    const { allowMissingBuiltWasm, verifyWasmAt } = await import("./integrity");
    const base = initOpts.wasmBasePath?.replace(/\/+$/, "");
    const wasmUrl = base ? `${base}/z_wasm_bg.wasm` : new URL("./generated/z_wasm_bg.wasm", import.meta.url).href;
    const integrityUrl = base ? `${base}/integrity.json` : new URL("./generated/integrity.json", import.meta.url).href;
    source = (await verifyWasmAt(wasmUrl, integrityUrl, { allowMissing: allowMissingBuiltWasm(!!base),
      onProgress: progress => reportEngineProgress({ component: "keys", ...progress }) })) ?? wasmUrl;
  }
  reportEngineProgress({ component: "keys", phase: "initialize" });
  await mod.default({ module_or_path: source });
  return mod;
}

async function loadMultiThread(threads: number): Promise<WasmBindings | null> {
  try {
    const mod = (await import("./generated-mt/z_wasm.js")) as unknown as WasmBindings;
    const { allowMissingBuiltWasm, verifyWasmAt } = await import("./integrity");
    const url = new URL("./generated-mt/z_wasm_bg.wasm", import.meta.url);
    const bytes = await verifyWasmAt(url.href, new URL("./generated-mt/integrity.json", import.meta.url).href, {
      allowMissing: allowMissingBuiltWasm(false),
      onProgress: progress => reportEngineProgress({ component: "keys", ...progress }),
    });
    reportEngineProgress({ component: "keys", phase: "initialize" });
    await mod.default({ module_or_path: bytes ?? url });
    if (typeof mod.initThreadPool !== "function") {
      return null;
    }
    await mod.initThreadPool(threads);
    return mod;
  } catch {
    console.warn("multicore z-wasm failed; using single-thread");
    reportEngineProgress({ component: "keys", phase: "fallback" });
    return null;
  }
}

async function loadBindings(): Promise<WasmBindings> {
  if (wasm) {
    return wasm;
  }
  if (wasmPromise) {
    return wasmPromise;
  }
  wasmPromise = loadBindingsInner().then((mod) => {
    if (initOpts.regtestNu63Height) mod.setRegtestNu63Height?.(initOpts.regtestNu63Height);
    if (initOpts.regtestNu7Height) mod.setRegtestNu7Height?.(initOpts.regtestNu7Height);
    return mod;
  });
  try {
    return await wasmPromise;
  } catch (e) {
    wasmPromise = null;
    throw e;
  }
}

async function loadBindingsInner(): Promise<WasmBindings> {
  const { configureWasmWorkerBasePath, configureRegtestNu63Height, configureRegtestNu7Height } = await import("./wasm-client");
  configureWasmWorkerBasePath(initOpts.wasmBasePath);
  configureRegtestNu63Height(initOpts.regtestNu63Height);
  configureRegtestNu7Height(initOpts.regtestNu7Height);
  const wantMt = initOpts.preferMulticore !== false && !initOpts.wasmModule && !initOpts.wasmBasePath && sabAvailable();
  const threads = resolveThreadCount();
  try {
    if (!initOpts.wasmModule) {
      const { canUseScanWorker } = await import("./scan-host");
      if (canUseScanWorker()) {
        const { attachWasmBindings, startScanWorker } = await import("./wasm-client");
        // Start scanner and key/UI bindings together; snapshot hydration waits
        // only for its own worker, not for two sequential WASM loads.
        // Do not block first paint on MT scan-worker wasm.
        void startScanWorker({
          threads,
          preferMulticore: wantMt,
          wasmBasePath: initOpts.wasmBasePath,
          regtestNu63Height: initOpts.regtestNu63Height,
          regtestNu7Height: initOpts.regtestNu7Height,
        });
        const st = await loadSingleThread();
        wasm = st;
        attachWasmBindings(st as never);
        runtime = {
          mode: "single-thread",
          threads: st.threadCount?.() ?? 1,
          sharedArrayBuffer: sabAvailable(),
          crossOriginIsolated: sabAvailable(),
          orchardCircuit: false,
          simd: simdFromCaps(st),
        };
        return st;
      }
    }
    if (wantMt) {
      const mt = await loadMultiThread(threads);
      if (mt) {
        wasm = mt;
        runtime = {
          mode: "multi-thread",
          threads: mt.threadCount?.() ?? threads,
          sharedArrayBuffer: true,
          crossOriginIsolated: true,
          orchardCircuit: true,
          simd: simdFromCaps(mt),
        };
        const { attachWasmBindings } = await import("./wasm-client");
        attachWasmBindings(mt as never);
        return mt;
      }
    }
    const st = await loadSingleThread();
    wasm = st;
    runtime = {
      mode: "single-thread",
      threads: st.threadCount?.() ?? 1,
      sharedArrayBuffer: typeof SharedArrayBuffer !== "undefined",
      crossOriginIsolated: typeof crossOriginIsolated !== "undefined" && crossOriginIsolated,
      orchardCircuit: false,
      simd: simdFromCaps(st),
    };
    const { attachWasmBindings } = await import("./wasm-client");
    attachWasmBindings(st as never);
    return st;
  } catch (e) {
    throw new Error(
      `Could not load the wallet engine. Check WASM assets, integrity manifests, worker URLs, and CSP. ${(e as Error).message}`,
      { cause: e },
    );
  }
}

/** Ledger helpers in z-wasm (APDU planning and parsing stay in Rust). */
export type LedgerWasm = {
  ledgerAppVersions: () => string;
  ledgerAppVersionAtLeast: (version: string, minimum: string) => boolean;
  ledgerAppInfoCommand: () => string;
  ledgerOpenAppCommand: () => string;
  ledgerDecodeAppInfo: (responseHex: string) => string;
  ledgerUfvkCommands: (accountIndex: number) => string;
  ledgerUfvkBytesRemaining: (responsesJson: string) => number;
  ledgerAccountFromResponses: (responsesJson: string, accountIndex: number, network: string) => string;
};

export function ledgerWasm(): LedgerWasm {
  const w = requireWasm() as unknown as Partial<LedgerWasm>;
  if (typeof w.ledgerUfvkCommands !== "function") {
    throw new Error("this wasm build has no hardware-wallet support; rebuild z-wasm");
  }
  return w as LedgerWasm;
}

function requireWasm(): WasmBindings {
  if (!wasm) {
    throw new Error("SDK not initialized — call initialize() first");
  }
  return wasm;
}

/**
 * Load z-wasm (idempotent). Required for {@link createWasmClient},
 * {@link generateMnemonic}, {@link deriveAccount}, {@link parseAddress}.
 * Not required for {@link createEngineClient}.
 */
export async function initialize(opts: SdkInitOptions = {}): Promise<WasmRuntime> {
  const requested = { ...opts };
  for (const key of ["regtestNu63Height", "regtestNu7Height"] as const) {
    const height = requested[key];
    if (height !== undefined && (!Number.isInteger(height) || height < (key === "regtestNu63Height" ? 2 : 3) || height > 0xffff_ffff)) {
      throw new WalletError("invalid_birthday", `Invalid ${key}.`);
    }
  }
  const nu63Height = requested.regtestNu63Height ?? ((wasm || wasmPromise) ? initOpts.regtestNu63Height : undefined) ?? 1_000_000;
  if (requested.regtestNu7Height !== undefined && requested.regtestNu7Height <= nu63Height) {
    throw new WalletError("invalid_birthday", "Regtest NU7 must activate after NU6.3.");
  }
  const effective = (options: SdkInitOptions, key: keyof SdkInitOptions) =>
    key === "threads" ? resolveThreadCount(options) :
    key === "preferMulticore" ? options.preferMulticore !== false :
    key === "regtestNu63Height" ? options.regtestNu63Height ?? 1_000_000 : options[key];
  if (requested.wasmBasePath) requested.wasmBasePath = new URL(requested.wasmBasePath,
    typeof document !== "undefined" ? document.baseURI : "http://localhost/").href.replace(/\/+$/, "");
  if (wasm || wasmPromise) {
    for (const key of ["wasmBasePath", "wasmModule", "threads", "preferMulticore", "regtestNu63Height", "regtestNu7Height"] as const) {
      const same = key === "wasmModule" && requested.wasmModule && initOpts.wasmModule
        ? sameModuleBytes(requested.wasmModule, initOpts.wasmModule) : effective(requested, key) === effective(initOpts, key);
      if (requested[key] !== undefined && !same) {
        throw new WalletError("busy", `The engine is already initialized with different ${key}. Reload to change engine configuration.`);
      }
    }
  }
  // First initialization owns the options; concurrent/repeated callers reuse
  // its verification, fetch and instantiation rather than re-downloading.
  if (!wasm && !wasmPromise) {
    initOpts = { ...requested, threads: resolveThreadCount(requested), preferMulticore: requested.preferMulticore !== false };
    // A worker's relative URL base differs from the page's. Resolve once before
    // dispatch so every surface verifies and instantiates the same artifacts.
    if (initOpts.wasmBasePath && typeof document !== "undefined") {
      initOpts.wasmBasePath = new URL(initOpts.wasmBasePath, document.baseURI).href.replace(/\/+$/, "");
    }
  }
  await loadBindings();
  reportEngineProgress({ component: "keys", phase: "ready" });
  runtimeChanged();
  const startScanner = !initOpts.wasmModule && canUseScanWorker() && !scanWorkerPresent() && !scanWorkerFailed();
  if (!scanWorkerPresent() && !startScanner) reportEngineProgress({ component: "scanner", phase: "ready" });
  const { prewarmProveWorker, prewarmOrchardProvingKey, startScanWorker } = await import("./wasm-client");
  // close() retires the scanner, while the verified page bindings remain cached.
  // A new owner needs a fresh scanner to hydrate its saved wallet. Reuse an
  // attached/starting worker; a failed generation still requires explicit load
  // recovery rather than replaying the operation that observed its failure.
  if (startScanner) {
    void startScanWorker({ threads: resolveThreadCount(),
      preferMulticore: initOpts.preferMulticore !== false && !initOpts.wasmBasePath && sabAvailable(),
      wasmBasePath: initOpts.wasmBasePath, regtestNu63Height: initOpts.regtestNu63Height, regtestNu7Height: initOpts.regtestNu7Height });
  }
  // Node tests pass `wasmModule` and have no Worker; skip so initialize stays milliseconds.
  if (initOpts.prewarmProveWorker !== false && !initOpts.wasmModule) {
    prewarmProveWorker();
  }
  if (initOpts.prewarmProvingKey && !initOpts.wasmModule) {
    void prewarmOrchardProvingKey();
  }
  return (
    wasmRuntime() ?? {
      mode: "single-thread",
      threads: 1,
      sharedArrayBuffer: false,
      crossOriginIsolated: false,
      orchardCircuit: false,
      simd: false,
    }
  );
}

function sameModuleBytes(left: BufferSource, right: BufferSource): boolean {
  const bytes = (value: BufferSource) => ArrayBuffer.isView(value)
    ? new Uint8Array(value.buffer, value.byteOffset, value.byteLength) : new Uint8Array(value);
  const a = bytes(left), b = bytes(right);
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function simdFromCaps(mod: WasmBindings): boolean {
  try {
    const caps = JSON.parse(mod.WasmWallet.capabilities()) as { simd?: boolean };
    return !!caps.simd;
  } catch {
    return false;
  }
}

export function wasmRuntime(): WasmRuntime | null {
  // Restore can replace a still-starting worker. Report the current generation
  // rather than retaining the initial worker's eventual result.
  const scanner = scanWorkerRuntime();
  if (scanner) return { ...scanner, scanner: "ready" };
  return runtime ? { ...runtime, scanner: scanWorkerPresent() ? "starting" : "main-thread" } : null;
}

export function generateMnemonic(): string {
  return requireWasm().generateMnemonic();
}

export function deriveAccount(
  mnemonic: string,
  network: Network = "mainnet",
  accountIndex = 0,
): Account {
  const a = requireWasm().accountFromMnemonic(mnemonic, network, accountIndex);
  return {
    network: a.network as Network,
    accountIndex: a.accountIndex,
    unifiedAddress: a.unifiedAddress,
    ufvk: a.ufvk,
    transparentAddress: a.transparentAddress ?? null,
  };
}

export function parseAddress(encoded: string): ParsedAddress {
  return requireWasm().parseAddress(encoded);
}

/**
 * True when `encoded` is a valid Zcash address (on `network`, if given).
 * Needs {@link initialize}.
 */
export function isValidAddress(encoded: string, network?: Network): boolean {
  try {
    const parsed = parseAddress(encoded.trim());
    if (network === undefined || parsed.network === network) return true;
    // Regtest reuses testnet's transparent prefixes (tm…/t2…), so those parse
    // as testnet. Unified, Sapling and TEX regtest addresses have their own.
    return network === "regtest" && parsed.network === "testnet" && (parsed.kind === "p2pkh" || parsed.kind === "p2sh");
  } catch {
    return false;
  }
}

/**
 * ZIP-32 seed fingerprint (64 hex digits) of a BIP-39 mnemonic: a stable,
 * non-secret id for "this seed", e.g. to recognise a returning wallet.
 * Needs {@link initialize}.
 */
export function seedFingerprint(mnemonic: string): string {
  const fn = requireWasm().seedFingerprint;
  if (typeof fn !== "function") throw new Error("this wasm build cannot derive seed fingerprints; rebuild z-wasm");
  return fn(mnemonic);
}

/** UA for `full` / `orchard` / `shielded` from a UFVK. Needs {@link initialize}. */
export function unifiedAddressForSet(ufvk: string, network: Network, set: UaReceiverSet): string {
  const fn = requireWasm().unifiedAddressForSet;
  if (typeof fn !== "function") {
    throw new Error("this wasm build cannot derive UA receiver sets");
  }
  return fn(ufvk, network, set);
}

export function canDeriveUaReceiverSet(): boolean {
  try {
    return typeof requireWasm().unifiedAddressForSet === "function";
  } catch {
    return false;
  }
}

/** Receivers on a UA / t-addr. Needs {@link initialize}. Native clients use their `inspectAddress()` method. */
export function inspectAddress(encoded: string): InspectedAddress {
  const fn = requireWasm().inspectAddress;
  if (typeof fn !== "function") {
    throw new Error("this wasm build cannot inspect addresses");
  }
  const raw = fn(encoded);
  const receivers = (raw.receivers ?? []).filter(
    (r): r is InspectedAddress["receivers"][number] =>
      r === "orchard" || r === "sapling" || r === "p2pkh",
  );
  const kind = raw.kind as InspectedAddress["kind"];
  const network = raw.network as InspectedAddress["network"];
  const receiverSet =
    raw.receiverSet === "full" || raw.receiverSet === "orchard" || raw.receiverSet === "shielded"
      ? raw.receiverSet
      : null;
  return { network, kind, receivers, receiverSet };
}

export function cryptoSmoke(): string {
  return requireWasm().cryptoSmoke();
}

export function defaultLightServer(network: Network): string {
  switch (network) {
    case "regtest":
      return REGTEST_ZAINO_GRPC;
    case "testnet":
      return "https://testnet.zec.rocks:443";
    default:
      return "https://zec.rocks:443";
  }
}
