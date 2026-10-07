/**
 * Hardware wallets (Keystone, Ledger). The spending key stays on the device:
 * the engine builds a PCZT for the send, proves it while the user reviews on
 * the device, applies the device's signatures (each one verified), and
 * broadcasts. All parsing and checking happens in Rust; a transport here only
 * moves bytes.
 *
 * ```ts
 * import TransportWebHID from "@ledgerhq/hw-transport-webhid";
 * const ledger = webHidLedger(await TransportWebHID.create());
 * await wallet.restoreHardware(await ledgerAccount(ledger, { network: "mainnet" }), "2024-06-01");
 * await wallet.send(to, "0.1", "thanks", { signer: ledgerSigner(ledger) });
 * ```
 */

import { WalletError } from "@z-stack/core";
import { ledgerWasm, type Network } from "./runtime";

export type HardwareDevice = "keystone" | "ledger";

/** A hardware-wallet account: what `wallet.restoreHardware` takes. */
export type HardwareAccount = {
  device: HardwareDevice;
  /** The unified full viewing key the device exported. */
  ufvk: string;
  /**
   * 64 hex. Keystone exports its ZIP-32 seed fingerprint (in the
   * `zcash-accounts` UR). Ledger does not; {@link ledgerAccount} supplies the
   * stand-in the engine expects.
   */
  seedFingerprint: string;
  accountIndex: number;
};

/** One Ledger APDU (`data` is hex). */
export type ApduCommand = { cla: number; ins: number; p1: number; p2: number; data: string };

/** Everything a Ledger needs to review and sign one transaction. */
export type LedgerSigningPlan = {
  commands: ApduCommand[];
  /** Index of the command whose response waits for the user's approval. */
  reviewIndex: number;
  signatures: { pool: "orchard" | "ironwood"; actionIndex: number }[];
};

/**
 * One APDU in, the raw response (status word included) out. Wrap a
 * `@ledgerhq/hw-transport` instance with {@link webHidLedger}.
 */
export type LedgerTransport = { exchange(apdu: Uint8Array): Promise<Uint8Array> };

export type HardwareSignStage = "connecting" | "reviewing" | "signing" | "proving" | "broadcasting";

export type LedgerSigner = {
  readonly device: "ledger";
  /** The running Zcash app's version. Throws `hardware_app` if the app is not open. */
  appVersion(): Promise<string>;
  /** Runs a plan; returns every raw response, stopping after the first error status. */
  exchange(plan: LedgerSigningPlan, onStage?: (stage: HardwareSignStage) => void): Promise<Uint8Array[]>;
};

export type KeystoneSigner = {
  readonly device: "keystone";
  /**
   * Show `pczt` to the device (animated QR, UR type `zcash-pczt`) and resolve
   * with the PCZT it signed. Reject with `hardwareCancelled()` if the user backs out.
   */
  sign(pczt: Uint8Array): Promise<Uint8Array>;
};

export type HardwareSigner = LedgerSigner | KeystoneSigner;

/** Options for `wallet.send(..., { signer })`. */
export type HardwareSendOptions = {
  signer: HardwareSigner;
  onStage?: (stage: HardwareSignStage) => void;
  signal?: AbortSignal;
  /** Revalidate user review/ownership immediately before submission. */
  beforeBroadcast?: () => boolean;
};

/** The error a signer rejects with when the user cancels (code `hardware_cancelled`). */
export function hardwareCancelled(message = "signing was cancelled"): WalletError {
  return new WalletError("hardware_cancelled", `hardware_cancelled: ${message}`);
}

/** A Keystone signer from your QR flow: show the PCZT, scan the signed one back. */
export function keystoneSigner(sign: (pczt: Uint8Array) => Promise<Uint8Array>): KeystoneSigner {
  return { device: "keystone", sign };
}

/**
 * Adapts a `@ledgerhq/hw-transport` transport (WebHID, WebUSB, Speculos).
 * Its `exchange` takes a Node-style Buffer; this passes one when `Buffer` exists.
 */
export function webHidLedger(transport: { exchange(apdu: never): Promise<Uint8Array> }): LedgerTransport {
  const B = (globalThis as { Buffer?: { from(b: Uint8Array): Uint8Array } }).Buffer;
  return {
    exchange: (apdu) => transport.exchange((B ? B.from(apdu) : apdu) as never),
  };
}

const hex = (b: Uint8Array) => Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
const unhex = (s: string) => new Uint8Array((s.match(/../g) ?? []).map((h) => parseInt(h, 16)));
const statusOf = (r: Uint8Array) => (r.length >= 2 ? (r[r.length - 2]! << 8) | r[r.length - 1]! : 0);

function encodeApdu(c: ApduCommand): Uint8Array {
  const data = unhex(c.data);
  if (data.length > 255) throw new Error("Ledger APDU payload exceeds 255 bytes");
  return new Uint8Array([c.cla, c.ins, c.p1, c.p2, data.length, ...data]);
}

const REVIEW_BUSY = 0x6901;
/** The app needs a moment after signing before it answers the next request. */
const SIGNING_COOLDOWN_MS = 4_000;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

type LedgerState = { tail: Promise<unknown>; readyAt: number };
const ledgerState = new WeakMap<LedgerTransport, LedgerState>();

/** One exchange at a time per transport, after any post-signing cooldown. */
function serialized<T>(transport: LedgerTransport, run: () => Promise<T>): Promise<T> {
  let state = ledgerState.get(transport);
  if (!state) ledgerState.set(transport, (state = { tail: Promise.resolve(), readyAt: 0 }));
  const s = state;
  const next = s.tail.then(async () => {
    const wait = s.readyAt - Date.now();
    if (wait > 0) await sleep(wait);
    return run();
  });
  s.tail = next.catch(() => undefined);
  return next;
}

async function send(transport: LedgerTransport, command: ApduCommand): Promise<Uint8Array> {
  const apdu = encodeApdu(command);
  // The device can answer "busy" before its review screen is up; replaying
  // only that rejected APDU is safe.
  for (let attempt = 0; ; attempt++) {
    const response = await transport.exchange(apdu);
    if (statusOf(response) !== REVIEW_BUSY || attempt === 2) return response;
    await sleep(200);
  }
}

function failFrom(message: string): WalletError {
  return WalletError.fromMessage(message);
}

async function runningApp(transport: LedgerTransport): Promise<{ name: string; version: string }> {
  const w = ledgerWasm();
  const response = await send(transport, JSON.parse(w.ledgerAppInfoCommand()) as ApduCommand);
  try {
    return JSON.parse(w.ledgerDecodeAppInfo(hex(response))) as { name: string; version: string };
  } catch (e) {
    throw failFrom(e instanceof Error ? e.message : String(e));
  }
}

async function requireZcashApp(transport: LedgerTransport, minimum: string): Promise<string> {
  const w = ledgerWasm();
  const { appName } = JSON.parse(w.ledgerAppVersions()) as { appName: string };
  const app = await runningApp(transport);
  if (app.name !== appName) {
    throw failFrom(`ledger_app_not_open: open the ${appName} app on your Ledger (it shows ${app.name || "the dashboard"})`);
  }
  if (!w.ledgerAppVersionAtLeast(app.version, minimum)) {
    throw failFrom(`ledger_app_outdated: update the Ledger ${appName} app to ${minimum} or later (found ${app.version})`);
  }
  return app.version;
}

/**
 * Reads the account's viewing key from a Ledger (the user approves on the
 * device). Ledger's Zcash app exports mainnet keys only.
 */
export async function ledgerAccount(
  transport: LedgerTransport,
  opts: { network: Network; accountIndex?: number },
): Promise<HardwareAccount> {
  const accountIndex = opts.accountIndex ?? 0;
  if (opts.network !== "mainnet") {
    throw failFrom("hardware_unsupported: the Ledger Zcash app exports mainnet accounts only");
  }
  const w = ledgerWasm();
  const { minAccount } = JSON.parse(w.ledgerAppVersions()) as { minAccount: string };
  return serialized(transport, async () => {
    await requireZcashApp(transport, minAccount);
    const { first, continuation } = JSON.parse(w.ledgerUfvkCommands(accountIndex)) as {
      first: ApduCommand;
      continuation: ApduCommand;
    };
    const responses = [hex(await send(transport, first))];
    try {
      for (let i = 0; i < 64 && w.ledgerUfvkBytesRemaining(JSON.stringify(responses)) > 0; i++) {
        responses.push(hex(await send(transport, continuation)));
      }
      const account = JSON.parse(
        w.ledgerAccountFromResponses(JSON.stringify(responses), accountIndex, opts.network),
      ) as { ufvk: string; seedFingerprint: string; accountIndex: number };
      return { device: "ledger", ...account };
    } catch (e) {
      throw failFrom(e instanceof Error ? e.message : String(e));
    }
  });
}

/** A Ledger signer over `transport` (see {@link webHidLedger}). */
export function ledgerSigner(transport: LedgerTransport): LedgerSigner {
  return {
    device: "ledger",
    appVersion: () =>
      serialized(transport, () =>
        requireZcashApp(transport, (JSON.parse(ledgerWasm().ledgerAppVersions()) as { minSigning: string }).minSigning),
      ),
    exchange: (plan, onStage) =>
      serialized(transport, async () => {
        const responses: Uint8Array[] = [];
        try {
          for (const [i, command] of plan.commands.entries()) {
            if (i === plan.reviewIndex) onStage?.("reviewing");
            if (i === plan.reviewIndex + 1) onStage?.("signing");
            const response = await send(transport, command);
            responses.push(response);
            if (statusOf(response) !== 0x9000) break;
          }
        } finally {
          // However signing ended, the app needs a moment before the next request.
          if (responses.length) ledgerState.get(transport)!.readyAt = Date.now() + SIGNING_COOLDOWN_MS;
        }
        return responses;
      }),
  };
}

/** Hex helpers the client uses to hand responses to the engine. */
export const hardwareHex = { hex, unhex };
