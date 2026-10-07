import type { WalletOptions } from "@z-stack/sdk";

// Public endpoint only. Disposable regtest tests replace this app-owned module.
export const connection: Pick<WalletOptions, "network" | "server"> = {
  network: "testnet",
  server: import.meta.env.VITE_ZSTACK_SERVER || "https://zcash-testnet.chainsafe.dev",
};
