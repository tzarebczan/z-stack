import type { Wallet } from "@z-stack/sdk";
/** Replaced by the scaffold only when --with-base is explicitly selected. */
export function BaseWallet(_props: {
  identity: string;
  disabled: boolean;
  withWallet: <T>(action: (wallet: Wallet) => Promise<T>) => Promise<T>;
}) {
  return null;
}
