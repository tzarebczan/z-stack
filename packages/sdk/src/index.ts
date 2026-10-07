/**
 * Browser wallet: local Rust/WASM keys, scanning and proofs. No registration,
 * custody, Cloudflare or fiat provider is selected. Configure the network once,
 * own the local storage namespace, and supply an optional unlocker.
 *
 * @module @z-stack/sdk
 * @example
 * ```ts
 * const wallet = await createWallet({ network: "testnet", server: "https://zcash-testnet.chainsafe.dev" });
 * if (!(await wallet.load())) {
 *   const { recoveryPhrase, wallet: state } = await wallet.create();
 *   // Show recoveryPhrase for backup before any fallible UI work. Do not log it.
 * }
 * await wallet.sync();
 * // From a separate user action, after review:
 * // await wallet.unlock(words);
 * // await wallet.send(recipient, "0.25");
 * // await wallet.close();
 * ```
 * Native engine: `@z-stack/sdk/native`. Hardware signers: `/hardware`.
 * Optional accounts/vaults: `/services`. Diagnostic raw clients: `/engine`.
 */

export { indexedDbWalletStorage, memoryWalletStorage, type WalletStorage, type WalletStorageTransaction } from "./storage";

export { createWallet, forgetWallet, lightServer, type Wallet, type WalletOptions, type WalletImportOptions, type WalletCreation, type WalletCreationPreparation, type WalletCreateOptions, type WalletUnlocker, type WalletUnlockRequest } from "./create-wallet";

export {
  SDK_VERSION,
  wasmRuntime,
  deriveAccount,
  seedFingerprint,
  parseAddress,
  isValidAddress,
  inspectAddress,
  unifiedAddressForSet,
  defaultLightServer,
  MAINNET_PUBLIC_LWD,
  TESTNET_PUBLIC_LWD,
  type Account,
  type AddressKind,
  type Network,
  type ParsedAddress,
  type SdkInitOptions,
  type WasmRuntime,
} from "./runtime";

export type { WasmProgress } from "./wasm-client";
export type {
  BalanceEvent, BirthdayInput, ChainTip, FeeEstimate, MaxSend, SpendResult,
  SendOptions, SyncEvent, WaitOpts, WalletEventHandler, WalletEventMap, WalletEventName,
} from "./engine";

export {
  grpcWebTransport,
  httpLwdTransport,
  // A custom transport's `submit` throws this for the node's explicit refusal.
  BroadcastRejection,
  type BlockTransport,
  type GrpcWebTransportOpts,
  type TransparentUtxo,
} from "./lwd";

// Pure helpers: amounts, ZIP-321, history, addresses, errors.
export {
  ZATOSHI_PER_ZEC,
  SHIELD_THRESHOLD_ZAT,
  BLOCK_SECONDS,
  canSend,
  canSendReason,
  canShield,
  classifyHistory,
  classifyHistoryList,
  filterHistory,
  historyStatusOf,
  historyActionLabel,
  inferTransactionType,
  computeDisplayValue,
  formatHistoryTime,
  formatZatoshis,
  parseZecToZatoshis,
  zecToZatoshis,
  parseZip321,
  zip321Uri,
  zip321UriMany,
  parseBirthdayInput,
  dateFromHeight,
  heightFromDate,
  catchUpPercent,
  displayCatchUpPercent,
  syncEta,
  shieldedAvailableZat,
  isWalletDataFinalStage,
  WALLET_ERROR_MESSAGES,
  WalletError,
  classifyWalletError,
  isWalletError,
  walletErrorMessage,
  DEFAULT_UA_RECEIVER_SET,
  UA_RECEIVERS,
  classifyUaReceiverSet,
  inspectAddressSummary,
  isUaReceiverSet,
  parseUaReceiverSet,
  uaIncludesTransparent,
  uaReceiverSetLabel,
  uaReceivers,
  type ClassifiedHistory,
  type HistoryEntry,
  type HistoryQuery,
  type HistoryStatus,
  type InspectedAddress,
  type SendBlockCode,
  type SendCheck,
  type SyncStage,
  type TxAction,
  type UaReceiver,
  type UaReceiverSet,
  type UnlockPolicy,
  type WalletErrorCode,
  type WalletSnapshot,
  type Zip321Extras,
  type Zip321Payment,
  type Zip321Request,
} from "@z-stack/core";
