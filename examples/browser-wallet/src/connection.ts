import type { WalletOptions } from "@z-stack/sdk";

// Public endpoint only. Disposable regtest tests replace this app-owned module.
export const connection: Pick<WalletOptions, "network" | "server" | "preferMulticore"> = {
  network: "testnet",
  // Set VITE_ZSTACK_MULTICORE=false to avoid downloading the threaded engine.
  preferMulticore: import.meta.env.VITE_ZSTACK_MULTICORE !== "false",
  server: import.meta.env.VITE_ZSTACK_SERVER || "https://zcash-testnet.chainsafe.dev",
};
