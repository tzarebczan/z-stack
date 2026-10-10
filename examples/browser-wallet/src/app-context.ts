import type { HistoryEntry, Wallet, WalletSnapshot } from "@z-stack/sdk";

/** Shared coordination only. Each flow owns its forms and transient state. */
export type WalletApp = {
  readonly wallet: Wallet;
  readonly unit: string;
  readonly snapshot: WalletSnapshot | undefined;
  readonly hasScanned: boolean;
  readonly unresolvedPayment: boolean;
  render(snapshot: WalletSnapshot): Promise<HistoryEntry[]>;
  run(action: () => Promise<void>, output?: HTMLElement): Promise<void>;
  updateControls(): void;
  clearSecrets(): void;
  reset(options?: { savedWallet: WalletSnapshot | null }): void;
};
