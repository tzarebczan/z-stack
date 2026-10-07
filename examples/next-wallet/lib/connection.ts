import type { WalletOptions } from "@z-stack/sdk";

// Only public chain configuration belongs here. Never put a phrase, key or
// authenticated backend token in a NEXT_PUBLIC environment variable.
export const connection: Pick<WalletOptions, "network" | "server"> = {
  network: "testnet",
  server: process.env.NEXT_PUBLIC_ZSTACK_SERVER || "https://zcash-testnet.chainsafe.dev",
};
