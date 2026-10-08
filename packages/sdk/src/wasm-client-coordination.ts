import {
  type ScanSession
} from "./scan-host";
import { type WalletOperation } from "./wallet-lifecycle";
import { type WalletGeneration } from "./wallet-storage";

/** Origin-wide actors and epochs shared by clients adopting the same session. */
export const runtimeState = {
  moduleSpendingSeed: null as string | null,
  originSpend: null as Promise<void> | null,
  persistOnHide: null as (() => Promise<void>) | null,
  hideBound: false as boolean,
  unbindHide: null as (() => void) | null,
  proveWorker: undefined as Worker | null | undefined,
  workerWasmBasePath: undefined as string | undefined,
  proveWorkerStarted: false as boolean,
  workerRegtestNu63: undefined as number | undefined,
  workerRegtestNu7: undefined as number | undefined,
  provePrewarm: null as Promise<unknown> | null,
  workerProvingKeyReady: false as boolean,
  wasmSyncEpoch: 0 as number,
  persistGen: 0 as number,
  syncAbort: new AbortController() as AbortController,
  proveSeq: 1 as number,
  scanWorkerKeyFor: null as ScanSession | null,
  liveWorkerWallet: null as { session: ScanSession; operation: WalletOperation; generation: WalletGeneration; } | null,
  spendingOperation: null as AbortSignal | null,
  workerHydration: null as { session: ScanSession; signal: AbortSignal; promise: Promise<void>; } | null,
};
