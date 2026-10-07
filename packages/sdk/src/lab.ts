/**
 * @z-stack/sdk/lab — everything in `@z-stack/sdk`, plus helpers for this
 * repo's lab UI, benchmarks and tests: loopback endpoints, scan-worker
 * controls, thread tuning, snapshot peeks and transport heuristics. Not a
 * stable integration surface.
 */

export * from "./index";
export * from "./engine-tools";
export * from "./hardware";
export * from "./services";

export {
  LOCAL_ZAINO_GRPC,
  LOCAL_ZAINO_GRPC_MAINNET,
  LOCAL_ZAINO_GRPC_WEB,
  LOCAL_ZAINO_GRPC_WEB as LOCAL_ZAINO_URL,
  LOCAL_GRPC_WEB,
  LOCAL_LWD_PIPE,
  REGTEST_ZAINO_GRPC,
  REGTEST_ZEBRA_RPC,
  LOCAL_ZAKURA_RPC_TESTNET,
  LOCAL_ZAKURA_RPC_MAINNET,
  localEndpoints,
  MAX_WASM_THREADS,
  DEFAULT_WASM_THREADS,
  hardwareThreadCount,
  threadsForCores,
  defaultThreadCount,
  canDeriveUaReceiverSet,
  cryptoSmoke,
  type LocalEndpoints,
} from "./runtime";

export {
  cancelWasmSync,
  peekSnapshotBytes,
  peekWasmWallet,
  prewarmProveWorker,
  setWasmSpendingSeed,
  wasmCapabilities,
} from "./wasm-client";

export {
  attachScanWorker,
  canUseScanWorker,
  forgetScanWorkerWallet,
  isTreeConflictError,
  localScanSession,
  prepareScanWorkerForNewWallet,
  restartScanWorker,
  scanWorkerBusy,
  scanWorkerReady,
  scanWorkerRuntime,
  scanWorkerStarting,
  treeConflictUserMessage,
  workerScanSession,
  type ApplySummary,
  type ScanRuntime,
  type ScanSession,
} from "./scan-host";

export {
  allowsTransparentQuery,
  isLoopbackUrl,
  isPublicLwdUrl,
  looksLikeGrpcWeb,
  looksLikeLwdPipe,
  PIPE_HTTP_MAX,
  splitProxyAuth,
  usesFastSync,
} from "./lwd";

export { createBlockPrefetch, type BlockPrefetch, type BlockPrefetchOpts, type PrefetchedRange } from "./prefetch";
export { PASSKEY_NO_SEED_BLOB, passkeyRpId, passkeyLocalhostUrl } from "./passkey";
export { REGTEST_FAUCET_MNEMONIC, REGTEST_FAUCET_TRANSPARENT } from "./constants";
export {
  FEE_PAD_ZAT,
  DATE_SAFETY_BLOCKS,
  MAX_SYNC_BLOCKS,
  syncTuning,
  typicalTip,
  historicOverlayVisible,
} from "@z-stack/core";

export {
  checkWalletStorageAdapter,
  checkVaultStoreAdapter,
  AdapterConformanceError,
  type AdapterTestFixture,
  type AdapterTestReport,
} from "./lab/adapter-conformance";
