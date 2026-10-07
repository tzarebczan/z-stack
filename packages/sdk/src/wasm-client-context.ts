import {
  type UnlockPolicy,
  type WalletSnapshot
} from "@z-stack/core";
import type { EventBus } from './events';
import type { BlockTransport } from "./lwd";
import { type PublicDataStatus } from "./public-data";
import {
  type ScanSession
} from "./scan-host";
import { type WalletOperation } from "./wallet-lifecycle";
import { type WalletGeneration } from "./wallet-storage";
import type * as runtime from './wasm-client-runtime';

/** Internal capabilities; each controller receives only its declared subset. */
export interface WasmClientContext {
  savedRevision: { source: ScanSession; revision: number; key: string; } | undefined;
  savedKeySeen: string | null;
  legacyBytes: Uint8Array<ArrayBufferLike> | null;
  replaceLegacyOnSave: boolean;
  storageGeneration: WalletGeneration | undefined;
  session: ScanSession | null;
  persistenceRequired: boolean;
  sessionOperation: WalletOperation | null;
  walletEpoch: number;
  stateEpoch: number;
  assertSource: (operation: WalletOperation, source: ScanSession) => void;
  unlockPolicy: UnlockPolicy;
  spendingSeed: string | null;
  network: runtime.Network;
  currentSession: () => ScanSession | null;
  loadLock: Promise<boolean> | null;
  report: (p: runtime.WasmProgress) => void;
  publishSession: (next: ScanSession, operation: WalletOperation, generation: WalletGeneration) => void;
  paintNoteBalance: (partial: runtime.WasmProgress) => Promise<void>;
  bindLocal: (handle: runtime.WasmWalletHandle, operation: WalletOperation, generation: WalletGeneration) => ScanSession;
  retireFailedSession: (stale: ScanSession) => void;
  opts: runtime.WasmClientOpts;
  onProgress: ((p: runtime.WasmProgress) => void) | undefined;
  bus: EventBus;
  snap: () => Promise<WalletSnapshot>;
  syncLock: { epoch: number; promise: Promise<WalletSnapshot>; } | null;
  memoLock: { epoch: number; promise: Promise<WalletSnapshot>; } | null;
  loadIfNeeded: () => Promise<boolean>;
  preferWorker: () => Promise<void>;
  transport: BlockTransport;
  memoFetch: "auto" | "on-demand" | "shared";
  persist: (lineage?: { epoch: number; baseKey: string | null; }, replacement?: runtime.ClaimedGeneration & { unlockPolicy?: UnlockPolicy; }, background?: boolean) => Promise<void>;
  transparentScan: "off" | "compact";
  localLight: boolean;
  syncOpts: { grpcWeb: boolean; lwdPipe: boolean; };
  grpcWeb: boolean;
  lwdPipe: boolean;
  prewarmProvingKey: boolean;
  client: runtime.WasmClient;
  rootsChecked: { source: ScanSession; at: number; } | null;
  refreshIfStale: (source: ScanSession, operation: WalletOperation) => Promise<void>;
  transparentScanStatus: PublicDataStatus;
  sharedMemoStatus: PublicDataStatus;
  selectiveMemoStatus: PublicDataStatus;
  memoAbort: AbortController | null;
  disposing: boolean;
  beginReservationDrain: () => (error?: unknown) => void;
  adoptSaved: (source: ScanSession, operation: WalletOperation) => Promise<void>;
  disposeWaits: AbortController;
  applyLoaded: (record: { key?: string; } | null | undefined, legacy: Uint8Array | null) => void;
  copyLegacy: (record: { bytes: Uint8Array; key?: string; } | null | undefined) => Uint8Array | null;
  refreshMempool: (doPersist?: boolean) => Promise<void>;
}
