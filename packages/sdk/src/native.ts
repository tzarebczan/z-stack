/** Native loopback engine. Does not load WASM or browser storage. */
import {
  createEngineClient, type EngineClient, type EngineClientOpts,
  type BirthdayInput, type CreateOpts, type RestoreOpts,
} from "./engine";
import type { WalletSnapshot } from "@z-stack/core";
export type NativeWalletOptions = EngineClientOpts;
export type NativeCreateOptions = Pick<CreateOpts, "server" | "validatorRpc" | "passphrase">;
export type NativeRestoreOptions = Pick<RestoreOpts, "server" | "validatorRpc" | "passphrase" | "unlockPolicy">;
export type NativeWallet = Omit<EngineClient,
  "restoreHardware" | "sendTransparent" | "estimateTransparentFee" | "supportsTransparentSend" |
  "hasSpendingSeed" | "create" | "restore" | "restoreUfvk" | "attachSeed" | "send"> & {
  create(network: string, birthday?: BirthdayInput, options?: NativeCreateOptions): Promise<WalletSnapshot>;
  restore(mnemonic: string, network: string, birthday?: BirthdayInput, options?: NativeRestoreOptions): Promise<WalletSnapshot>;
  restoreUfvk(ufvk: string, network: string, birthday?: BirthdayInput, options?: NativeRestoreOptions): Promise<WalletSnapshot>;
  attachSeed(mnemonic: string): Promise<WalletSnapshot>;
  send(to: string, amountZec: string, memo?: string): ReturnType<EngineClient["send"]>;
};
export function createNativeWallet(baseUrl: string, options?: NativeWalletOptions): NativeWallet {
  const { restoreHardware, sendTransparent, estimateTransparentFee,
    supportsTransparentSend, hasSpendingSeed, ...client } = createEngineClient(baseUrl, options);
  return client;
}
export { probeEngine, type EngineHealth, type LightProbe, type SetupProbe, type ValidatorProbe } from "./engine";
export { NATIVE_BRIDGE_URL } from "./runtime";
