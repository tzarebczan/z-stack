/** Internal engine/lab integration surface. Not the stable browser Wallet API. */
export * from "./engine";
export { createEngineClient as createNativeClient } from "./engine";
export { initialize, generateMnemonic, wasmRuntime, NATIVE_BRIDGE_URL } from "./runtime";
export { createWasmClient, forgetWasmWallet, orchardProvingKeyReady, prewarmOrchardProvingKey,
  type WasmClient, type WasmClientOpts } from "./wasm-client";
