const messages = {
  rpc_unavailable: "The Base network is unavailable. Try checking again.",
  cancelled: "The Base payment was cancelled.",
  wallet_changed: "The wallet changed. Review the payment again.",
  chain_mismatch: "The Base network could not be verified.",
  invalid_payment: "Enter a valid Base recipient and amount.",
  invalid_configuration: "Choose an explicit HTTPS Base RPC, or HTTP on loopback for development.",
  invalid_response: "The Base network returned invalid fee estimates.",
  verification_failed:
    "The saved transaction status could not be verified. Check again before sending.",
  insufficient_funds: "There is not enough balance for the payment and network fee.",
  fee_changed: "The Base network fee increased. Review the new fee before sending.",
  review_invalid:
    "Use the original review from this Base wallet. Already signed reviews cannot be reused.",
  review_expired: "The Base payment review expired. Review it again.",
  payment_blocked: "A saved payment or another wallet action is blocking this payment.",
  unlock_failed: "The wallet could not be unlocked. Check your recovery phrase or sign-in method.",
  signing_failed: "The Base payment could not be signed.",
  transaction_mismatch: "The signer changed the reviewed Base transaction.",
  storage_failed: "The payment status could not be saved. Nothing new was submitted.",
  submission_unknown:
    "The submission acknowledgement is unknown. Check the saved transaction hash before sending again.",
  submission_not_sent: "The payment was not sent. Review a new payment when ready.",
} as const;
export type BaseErrorCode = keyof typeof messages;

/** Safe public copy. Causes are deliberately excluded from ordinary JSON/event serialization. */
export class BaseError extends Error {
  constructor(
    readonly code: BaseErrorCode,
    cause?: unknown,
  ) {
    super(messages[code]);
    this.name = "BaseError";
    if (cause !== undefined)
      Object.defineProperty(this, "cause", { value: cause, enumerable: false });
  }
}
export function baseErrorMessage(error: unknown): string {
  return error instanceof BaseError
    ? messages[error.code]
    : "The Base action could not be completed.";
}
export async function baseOperation<T>(
  run: () => Promise<T>,
  code: BaseErrorCode = "rpc_unavailable",
): Promise<T> {
  try {
    return await run();
  } catch (error) {
    if (error instanceof BaseError) throw error;
    throw new BaseError(
      error instanceof DOMException && error.name === "AbortError" ? "cancelled" : code,
      error,
    );
  }
}
