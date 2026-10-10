/**
 * Stable wallet error codes. UIs must switch on `code`, not parse messages.
 * Keep lockstep with `z_engine::EngineError` plus common bridge / WASM strings.
 */

export type WalletErrorCode =
  | "unknown"
  | "cancelled"
  | "busy"
  | "closed"
  | "not_initialized"
  | "not_found"
  | "already_exists"
  | "no_account"
  | "sync_required"
  | "insufficient_funds"
  | "missing_params"
  | "seed_mismatch"
  | "seed_locked"
  | "view_only"
  | "seed_decrypt_failed"
  | "chain_mismatch"
  | "deep_sync_rejected"
  | "birthday_above_tip"
  | "invalid_birthday"
  | "invalid_recovery_phrase"
  | "forget_pending"
  | "rescan_pending"
  | "rescan_later_birthday"
  | "unsupported_payment_uri"
  | "broadcast_rejected"
  | "broadcast_failed"
  | "reorg"
  | "transport"
  | "wallet_db"
  | "storage_full"
  | "invalid_network"
  | "invalid_address"
  | "invalid_amount"
  | "invalid_memo"
  | "auth"
  | "unsupported_transparent"
  | "unsupported_destination"
  | "hardware_rejected"
  | "hardware_locked"
  | "hardware_app"
  | "hardware_unsupported"
  | "hardware_mismatch"
  | "hardware_cancelled"
  | "wallet_changed";

/** User-facing copy keyed by {@link WalletErrorCode}. UIs must not parse `message`. */
export const WALLET_ERROR_MESSAGES: Record<WalletErrorCode, string> = {
  unknown: "Something went wrong.",
  cancelled: "Wallet operation cancelled.",
  busy: "Another wallet operation is in progress.",
  closed: "This wallet client is closed.",
  not_initialized: "Initialize the wallet engine first.",
  not_found: "No wallet on this device.",
  already_exists: "A wallet already exists here.",
  no_account: "This wallet has no account yet.",
  sync_required: "Sync the wallet before sending.",
  insufficient_funds: "Not enough shielded funds for this send (including the fee).",
  missing_params: "Required proving parameters are unavailable. Check the wallet setup.",
  seed_mismatch: "This recovery phrase does not match your wallet.",
  seed_locked: "Unlock spending to send or shield funds.",
  view_only: "This wallet can show activity but cannot spend.",
  seed_decrypt_failed: "Could not decrypt the seed. Check the passphrase.",
  chain_mismatch: "This wallet does not match the light server chain.",
  deep_sync_rejected: "Birthday is too far below tip for a default sync.",
  invalid_recovery_phrase: "Those words are not a valid recovery phrase.",
  invalid_birthday: "Enter a positive block height or a valid date in YYYY-MM-DD format.",
  forget_pending: "Sync to confirm or expire pending payments before removing this wallet.",
  rescan_pending: "Sync to confirm or expire your pending payment before rescanning.",
  rescan_later_birthday: "Choose a height or date at or before the wallet’s current birthday.",
  unsupported_payment_uri: "Paste the recipient address itself. This form does not accept zcash: payment links.",
  birthday_above_tip: "Birthday is above the current chain tip.",
  broadcast_rejected: "The network rejected this transaction.",
  broadcast_failed: "Submission may have succeeded. Check the transaction ID before sending another payment.",
  wallet_changed: "The wallet was updated in another tab and has been reloaded. Try again.",
  reorg: "The chain reorganized. Rescan to continue.",
  transport: "Could not reach the light server.",
  wallet_db: "Could not read or update wallet data. Keep your recovery backup and try again.",
  storage_full: "Could not save recent wallet progress: browser storage is full. Free device space and retry. Keep this site's data and your recovery backup.",
  invalid_network: "Unknown network.",
  invalid_address: "That address is not a valid Zcash destination.",
  invalid_amount: "Enter a valid amount with up to eight decimal places.",
  invalid_memo: "Memo is too long.",
  auth: "This wallet action is not authorized.",
  unsupported_transparent: "Transparent send is not supported. Shield first.",
  unsupported_destination:
    "Need a shielded receiver. Transparent-only destinations are not supported.",
  hardware_rejected: "The transaction was rejected on the device.",
  hardware_locked: "Unlock your hardware wallet and try again.",
  hardware_app: "Open the Zcash app on your hardware wallet (update it if asked).",
  hardware_unsupported: "Your hardware wallet cannot sign this transaction.",
  hardware_mismatch: "This hardware wallet does not hold this account's keys.",
  hardware_cancelled: "Signing was cancelled.",
};

/** Friendly string for a code. `unknown` keeps `fallback` when present. */
export function walletErrorMessage(code: WalletErrorCode, fallback?: string): string {
  if (code === "unknown") {
    const f = fallback?.trim();
    return f || WALLET_ERROR_MESSAGES.unknown;
  }
  return WALLET_ERROR_MESSAGES[code];
}

export class WalletError extends Error {
  readonly code: WalletErrorCode;
  override readonly cause?: unknown;
  /**
   * `broadcast_failed`: the transaction's id (display-order hex). It stays
   * available even after cancellation, close or replacement. The original
   * wallet may have been forgotten; never infer rejection from that change.
   */
  readonly txid?: string;

  constructor(code: WalletErrorCode, message: string, cause?: unknown, details?: { txid?: string }) {
    super(message);
    this.name = "WalletError";
    this.code = code;
    this.cause = cause;
    if (details?.txid) this.txid = details.txid;
  }

  /** Fixed display copy, including unknown failures. Raw message/cause are for private debugging. */
  userMessage(): string {
    return walletErrorMessage(this.code);
  }

  static fromUnknown(err: unknown): WalletError {
    if (err instanceof WalletError) return err;
    if (err && typeof err === "object" && "name" in err && err.name === "AbortError") {
      return new WalletError("cancelled", WALLET_ERROR_MESSAGES.cancelled, err);
    }
    if (err && typeof err === "object" && "name" in err && err.name === "QuotaExceededError") {
      return new WalletError("storage_full", WALLET_ERROR_MESSAGES.storage_full, err);
    }
    const message = err instanceof Error ? err.message : String(err);
    return WalletError.fromMessage(message, err);
  }

  static fromMessage(message: string, cause?: unknown): WalletError {
    return new WalletError(classifyWalletError(message), message, cause);
  }
}

export function isWalletError(err: unknown): err is WalletError {
  return err instanceof WalletError;
}

/** Map a known engine / bridge / WASM string to a stable code. Unknown → `unknown`. */
export function classifyWalletError(message: string): WalletErrorCode {
  const m = message.trim();
  const lower = m.toLowerCase();

  if (lower === "sync cancelled") return "cancelled";

  if (lower.includes("invalid recovery phrase:") || lower.includes("invalid mnemonic:")) return "invalid_recovery_phrase";

  if (lower.includes("those words do not match this wallet\'s viewing key")) return "seed_mismatch";

  // Hardware wallets: stable prefixes from the engine and the SDK signers.
  const status = /ledger_status_([0-9a-f]{4})/.exec(lower)?.[1];
  if (status === "6985" || status === "5501" || status === "6a80") return "hardware_rejected";
  if (status && ["5515", "6982", "5303", "63c0", "5502"].includes(status)) return "hardware_locked";
  if (/ledger_app_(outdated|not_open)|ledger_status_(6807|6d00|6e00)/.test(lower)) return "hardware_app";
  if (/ledger_(memo_hash_unsupported|legacy_orchard_recovery_unsupported|capacity)|hardware_unsupported/.test(lower)) {
    return "hardware_unsupported";
  }
  if (/ledger_signature_mismatch|hardware_mismatch|seed fingerprint does not match|does not belong to account/.test(lower)) {
    return "hardware_mismatch";
  }
  if (lower.startsWith("hardware_cancelled")) return "hardware_cancelled";
  if (/(changed|updated|saved) (in|by) another tab/.test(lower)) return "wallet_changed";
  if (/bridge token|401 |unauthorized|not a zaino password/.test(lower)) return "auth";
  if (lower.includes("origin not allowed")) return "auth";
  if (lower.includes("insufficient") || lower.includes("no spendable")) return "insufficient_funds";
  if (lower.includes("sync required") || lower.includes("still behind tip") || lower.includes("must scan blocks first")) return "sync_required";
  if (
    lower.includes("seed unlock") ||
    lower.includes("seedlocked") ||
    /re-enter spending seed/.test(lower)
  ) {
    return "seed_locked";
  }
  if (lower.includes("view-only") || lower.includes("view only")) return "view_only";
  if (lower.includes("seed decrypt") || lower.includes("wrong passphrase"))
    return "seed_decrypt_failed";
  if (lower.includes("already exists")) return "already_exists";
  if (
    lower.includes("wallet not found") ||
    lower.includes("no wasm wallet") ||
    lower.includes("no account")
  ) {
    return lower.includes("no account") ? "no_account" : "not_found";
  }
  if (lower.includes("no account")) return "no_account";
  if (lower.includes("proving parameters missing") || lower.includes("missing params")) {
    return "missing_params";
  }
  if (lower.includes("rescan pending payment")) return "rescan_pending";
  if (lower.includes("rescan later birthday")) return "rescan_later_birthday";
  if (lower.includes("invalid birthday")) return "invalid_birthday";
  if (lower.includes("chain mismatch")) return "chain_mismatch";
  if (lower.includes("too far below tip") || lower.includes("deep sync"))
    return "deep_sync_rejected";
  if (lower.includes("above chain tip") || lower.includes("above tip")) return "birthday_above_tip";
  if (lower.includes("broadcast rejected")) return "broadcast_rejected";
  if (lower.includes("broadcast failed") || lower.includes("broadcast outcome unknown")) return "broadcast_failed";
  if (lower.startsWith("reorg ") || lower.includes("reorg at height")) return "reorg";
  if (
    lower.includes("grpc/transport") ||
    lower.includes("i/o:") ||
    lower.includes("engine not reachable") ||
    lower.includes("failed to fetch") ||
    lower.includes("networkerror")
  ) {
    return "transport";
  }
  if (lower.includes("wallet db")) return "wallet_db";
  if (lower.includes("invalid network") || lower.includes("unknown network"))
    return "invalid_network";
  if (
    lower.includes("invalid address") ||
    lower.includes("not a zcash:") ||
    lower.includes("not uri-safe") ||
    lower.includes("empty address") ||
    lower.includes("no address")
  ) {
    return "invalid_address";
  }
  if (
    lower.includes("empty amount") ||
    lower.includes("decimal zec") ||
    lower.includes("more than 8 decimal") ||
    lower.includes("amount must be greater")
  ) {
    return "invalid_amount";
  }
  if (lower.includes("memo longer") || lower.includes("memo:")) return "invalid_memo";
  if (lower.includes("transparent send is not supported")) return "unsupported_transparent";
  if (
    lower.includes("orchard-only") ||
    lower.includes("no shielded receiver") ||
    lower.includes("sapling destinations")
  ) {
    return "unsupported_destination";
  }
  return "unknown";
}
