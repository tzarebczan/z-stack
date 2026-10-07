/**
 * Hardware-wallet engine calls, shared by the scan worker and the in-process
 * session so both run the same code against a wasm wallet.
 */

export type HardwareAction =
  | "createSend"
  | "prove"
  | "signerCopy"
  | "applySignatures"
  | "combine"
  | "finalize"
  | "releaseLocks"
  | "rollback"
  | "ledgerPlan"
  | "ledgerApply"
  | "account";

export type HardwareArgs = {
  pczt?: Uint8Array;
  /** The second PCZT for `combine` (the device-signed one). */
  signed?: Uint8Array;
  to?: string;
  amountZec?: string;
  memo?: string;
  copy?: "full" | "compact" | "batch";
  /** `applySignatures`: signature list; `ledgerApply`: hex responses. JSON. */
  json?: string;
  appVersion?: string;
  /** `rollback`: the finalized transaction to abandon (display-order hex). */
  txid?: string;
};

export type HardwareResult = { bytes?: Uint8Array; json?: string; n?: number };

export type HardwareWalletHandle = {
  hardwareAccount?: () => string | undefined;
  hardwareCreateSend?: (to: string, amountZec: string, memo?: string) => Uint8Array;
  hardwareFinalize?: (pczt: Uint8Array) => string;
  hardwareReleaseLocks?: () => number;
  abandon?: (txid: string) => boolean;
  ledgerSigningPlan?: (pczt: Uint8Array, appVersion: string) => string;
  ledgerApplyResponses?: (pczt: Uint8Array, responsesJson: string) => Uint8Array;
};

export type HardwareStatics = {
  hardwareProve?: (pczt: Uint8Array) => Uint8Array;
  hardwareSignerCopy?: (pczt: Uint8Array, copy: string) => Uint8Array;
  hardwareApplySignatures?: (pczt: Uint8Array, json: string) => Uint8Array;
  hardwareCombine?: (proved: Uint8Array, signed: Uint8Array) => Uint8Array;
};

function need<T>(fn: T | undefined, what: string): T {
  if (typeof fn !== "function") throw new Error(`this wasm build has no hardware-wallet support (${what}); rebuild z-wasm`);
  return fn;
}

function pcztOf(args: HardwareArgs): Uint8Array {
  if (!args.pczt) throw new Error("hardware call needs a PCZT");
  return args.pczt;
}

export function runHardware(
  w: HardwareWalletHandle,
  statics: HardwareStatics,
  action: HardwareAction,
  args: HardwareArgs,
): HardwareResult {
  switch (action) {
    case "account":
      return { json: w.hardwareAccount?.() ?? "null" };
    case "createSend":
      return { bytes: need(w.hardwareCreateSend, action).call(w, args.to ?? "", args.amountZec ?? "0", args.memo) };
    case "prove":
      return { bytes: need(statics.hardwareProve, action)(pcztOf(args)) };
    case "signerCopy":
      return { bytes: need(statics.hardwareSignerCopy, action)(pcztOf(args), args.copy ?? "full") };
    case "applySignatures":
      return { bytes: need(statics.hardwareApplySignatures, action)(pcztOf(args), args.json ?? "[]") };
    case "combine":
      if (!args.signed) throw new Error("combine needs the signed PCZT");
      return { bytes: need(statics.hardwareCombine, action)(pcztOf(args), args.signed) };
    case "finalize":
      return { json: need(w.hardwareFinalize, action).call(w, pcztOf(args)) };
    case "releaseLocks":
      return { n: need(w.hardwareReleaseLocks, action).call(w) };
    case "rollback": {
      // Undo only this send: its reservations, and its pending transaction
      // if it was finalized but never broadcast.
      const n = need(w.hardwareReleaseLocks, action).call(w);
      if (args.txid) need(w.abandon, action).call(w, args.txid);
      return { n };
    }
    case "ledgerPlan":
      return { json: need(w.ledgerSigningPlan, action).call(w, pcztOf(args), args.appVersion ?? "") };
    case "ledgerApply":
      return { bytes: need(w.ledgerApplyResponses, action).call(w, pcztOf(args), args.json ?? "[]") };
    default:
      throw new Error(`unknown hardware action ${String(action)}`);
  }
}
