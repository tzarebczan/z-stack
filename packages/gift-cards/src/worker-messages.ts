import type { GiftCard, GiftNetwork } from "./protocol";
import type { GiftAccount, GiftSpendResult, GiftTransaction, GiftWalletSnapshot } from "./service";
export type BrowserGiftOptions = {
  network: GiftNetwork; server: string; namespace: string;
  regtestNu63Height?: number; regtestNu7Height?: number; wasmBasePath?: string;
  deepSync?: boolean; lightServerGraceMs?: number;
  onProgress?: (progress: { stage: string; percent?: number }) => void;
};
export type WorkerConfiguration = Omit<BrowserGiftOptions, "onProgress">;
export type GiftWorkerRequest = { id: number } & (
  | { op: "create"; config: WorkerConfiguration }
  | { op: "derive"; config: WorkerConfiguration; card: GiftCard }
  | { op: "open"; config: WorkerConfiguration; card: GiftCard }
  | { op: "snapshot" | "sync" | "history" | "close" }
  | { op: "transaction"; txid: string }
  | { op: "fee"; to: string; amountZec: string }
  | { op: "send"; to: string; amountZec: string; maxFeeZat: string }
  | { op: "reserved-ack"; accepted: boolean }
);
export type GiftWorkerReply =
  | { kind: "account"; id: number; value: GiftAccount }
  | { kind: "snapshot"; id: number; value: GiftWalletSnapshot }
  | { kind: "history"; id: number; value: GiftTransaction[] }
  | { kind: "transaction"; id: number; value: GiftTransaction | null }
  | { kind: "fee"; id: number; value: { feeZat: number } }
  | { kind: "spend"; id: number; value: GiftSpendResult }
  | { kind: "done"; id: number }
  | { kind: "error"; id: number; code: string; txid?: string }
  | { kind: "reserved"; id: number; txid: string }
  | { kind: "progress"; stage: string; percent?: number };
