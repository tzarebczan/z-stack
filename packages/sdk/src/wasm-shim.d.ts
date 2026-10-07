declare module "./generated/z_wasm.js" {
  const init: (module_or_path?: unknown) => Promise<unknown>;
  export default init;
  export function generateMnemonic(): string;
  export function seedFingerprint(mnemonic: string): string;
  export function accountFromMnemonic(
    mnemonic: string,
    network: string,
    accountIndex: number,
  ): {
    network: string;
    accountIndex: number;
    unifiedAddress: string;
    ufvk: string;
    transparentAddress?: string;
  };
  export function parseAddress(encoded: string): {
    network: string;
    kind: string;
  };
  export function inspectAddress(encoded: string): {
    network: string;
    kind: string;
    receivers?: string[];
    receiverSet?: string | null;
  };
  export function unifiedAddressForSet(ufvk: string, network: string, set: string): string;
  export function ufvkCovers(network: string, derived: string, stored: string): void;
  export function checkViewingKey(network: string, ufvk: string): void;
  export function warmOrchardProvingKey(): boolean;
  export function orchardProvingKeyReady(): boolean;
  export function cryptoSmoke(): string;
  export class WasmEngine {
    constructor(network: string, serverUrl?: string);
    network(): string;
    serverUrl(): string;
  }
  export class WasmWallet {
    static create(
      network: string,
      mnemonic: string,
      birthday: number,
      accountIndex: number,
    ): WasmWallet;
    static fromSnapshot(bytes: Uint8Array): WasmWallet;
    static fromUfvk(
      network: string,
      ufvk: string,
      birthday: number,
      accountIndex: number,
    ): WasmWallet;
    static proveError(): string;
    static shieldError(): string;
    static capabilities(): string;
    estimateFee(to: string, amountZec: string, memo?: string): string;
    estimateTransparentFee(to: string, amountZec: string): string;
    proveTransparentSend(mnemonic: string, to: string, amountZec: string, maxFeeZat?: string): string;
    maxSend(to?: string): string;
    proveSend(mnemonic: string, to: string, amountZec: string, memo?: string): string;
    proveShield(mnemonic: string, thresholdZat: number): string;
    applyUtxos(json: string): number;
    applyMempool(json: string): number;
    enhanceRawTx(hex: string): number;
    rewindTo(height: number): number;
    abandon(txid: string): boolean;
    applyMinedTx(hex: string, time: number): string;
    toSnapshot(): Uint8Array;
    applyCompactBlock(bytes: Uint8Array): string;
    applyCompactBlocks(blob: Uint8Array): string;
    applyCompactBlocksSummary(blob: Uint8Array): string;
    applyTreeState(json: string): void;
    applySubtreeRoots(protocol: string, json: string): number;
    treesReady(): boolean;
    sinsemillaLive(): boolean;
    subtreeRootCount(protocol: string): number;
    subtreeRootsStart(protocol: string): number;
    history(limit: number): string;
    snapshotJson(server: string): string;
    scannedHeight(): number;
    birthday(): number;
    nextHeight(): number;
    unifiedAddress(): string;
    transparentAddress(): string | undefined;
    nextUnifiedAddress(): string;
    attachSeed(mnemonic: string): void;
    recomputePools(): void;
    recomputePoolsWithTick?(cb: (hashed: number, total: number, message: string) => void): void;
    resetScan(): void;
    warmOrchardProvingKey(): boolean;
    orchardProvingKeyReady(): boolean;
    free(): void;
  }
}

declare module "./generated-mt/z_wasm.js" {
  const init: (module_or_path?: unknown) => Promise<unknown>;
  export default init;
  export function initThreadPool(num_threads: number): Promise<void>;
  export function generateMnemonic(): string;
  export function seedFingerprint(mnemonic: string): string;
  export function threadCount(): number;
  export function warmOrchardProvingKey(): boolean;
  export function orchardProvingKeyReady(): boolean;
  export function inspectAddress(encoded: string): {
    network: string;
    kind: string;
    receivers?: string[];
    receiverSet?: string | null;
  };
  export function unifiedAddressForSet(ufvk: string, network: string, set: string): string;
  export class WasmWallet {
    static create(
      network: string,
      mnemonic: string,
      birthday: number,
      accountIndex: number,
    ): WasmWallet;
    static fromSnapshot(bytes: Uint8Array): WasmWallet;
    static fromUfvk(
      network: string,
      ufvk: string,
      birthday: number,
      accountIndex: number,
    ): WasmWallet;
    static proveError(): string;
    static shieldError(): string;
    static capabilities(): string;
    estimateFee(to: string, amountZec: string, memo?: string): string;
    estimateTransparentFee(to: string, amountZec: string): string;
    proveTransparentSend(mnemonic: string, to: string, amountZec: string, maxFeeZat?: string): string;
    maxSend(to?: string): string;
    proveSend(mnemonic: string, to: string, amountZec: string, memo?: string): string;
    proveShield(mnemonic: string, thresholdZat: number): string;
    applyUtxos(json: string): number;
    applyMempool(json: string): number;
    enhanceRawTx(hex: string): number;
    rewindTo(height: number): number;
    abandon(txid: string): boolean;
    applyMinedTx(hex: string, time: number): string;
    toSnapshot(): Uint8Array;
    applyCompactBlock(bytes: Uint8Array): string;
    applyCompactBlocks(blob: Uint8Array): string;
    applyCompactBlocksSummary(blob: Uint8Array): string;
    applyTreeState(json: string): void;
    applySubtreeRoots(protocol: string, json: string): number;
    treesReady(): boolean;
    sinsemillaLive(): boolean;
    subtreeRootCount(protocol: string): number;
    subtreeRootsStart(protocol: string): number;
    history(limit: number): string;
    snapshotJson(server: string): string;
    scannedHeight(): number;
    birthday(): number;
    nextHeight(): number;
    unifiedAddress(): string;
    transparentAddress(): string | undefined;
    nextUnifiedAddress(): string;
    attachSeed(mnemonic: string): void;
    recomputePools(): void;
    recomputePoolsWithTick?(cb: (hashed: number, total: number, message: string) => void): void;
    resetScan(): void;
    free(): void;
  }
}
