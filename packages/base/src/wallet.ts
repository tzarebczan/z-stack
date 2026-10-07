import {
  createPublicClient,
  getAddress,
  http,
  isHash,
  parseTransaction,
  recoverTransactionAddress,
  type Address,
  type Hash,
} from "viem";
import type { HDAccount } from "viem/accounts";
import { freezeBaseNetwork, type BaseNetwork, type BaseChain } from "./network";
import { createBaseTransfers } from "./transfers";
import { BaseError, baseOperation } from "./errors";
import { baseTransactionRecord, type BaseTransactionRecord } from "./transaction";
import type { BaseBalance } from "./account";

export interface BaseSessionGuard {
  readonly signal?: AbortSignal;
  /** Throw if the account, opt-in, auth session or screen intent changed. */
  assertCurrent(): void;
}
export interface BasePaymentReview {
  readonly asset: "eth" | "usdc";
  readonly owner: Address;
  readonly recipient: Address;
  readonly amount: bigint;
  readonly chainId: number;
  /** Current total estimate including variable L1/operator charges; not a total spending ceiling. */
  readonly fee: bigint;
  /** Execution fee limit bound by the signed gas and maxFeePerGas fields. */
  readonly l2Max: bigint;
  readonly deadline: number;
}
export interface BaseSubmission extends BaseTransactionRecord {
  readonly review: BasePaymentReview;
}
export class BaseSubmissionUnknownError extends BaseError {
  readonly hash: Hash;
  constructor(readonly submission: BaseSubmission) {
    super("submission_unknown");
    this.name = "BaseSubmissionUnknownError";
    Object.defineProperty(this, "submission", { value: submission, enumerable: false });
    this.hash = submission.hash;
  }
}
export class BaseSubmissionNotSentError extends BaseError {
  constructor(
    readonly submission: BaseSubmission,
    cause?: unknown,
  ) {
    super("submission_not_sent", cause);
    this.name = "BaseSubmissionNotSentError";
    Object.defineProperty(this, "submission", { value: submission, enumerable: false });
  }
}
export interface BaseSendOptions {
  readonly guard: BaseSessionGuard;
  /** Shared by all tabs and all native/sponsored payments for this address. */
  withSpendLock<T>(run: () => Promise<T>): Promise<T>;
  /** Fresh unlock. Do not retain this signer after the callback returns. */
  withSigner<T>(run: (signer: HDAccount) => Promise<T>): Promise<T>;
  /** Must check durable pending native AND sponsored reservations while holding the lock. */
  assertNoPending(): void | Promise<void>;
  /** Persist before broadcast. Reject on failure; unknown outcomes must retain this reservation. */
  reserveSubmission(submission: BaseSubmission): Promise<void>;
  /** Remove only this matching record when the local transport was never invoked, while holding the same lock. */
  releaseSubmission(submission: BaseSubmission): Promise<void>;
}
const balanceAbi = [
  {
    type: "function",
    name: "balanceOf",
    stateMutability: "view",
    inputs: [{ name: "account", type: "address" }],
    outputs: [{ name: "", type: "uint256" }],
  },
] as const;

/** Optional, no storage, auth, telemetry, network polling or implicit account registration. */
export function createBaseWallet(options: {
  address: Address;
  network: BaseChain;
  rpcUrl: string;
}) {
  if (typeof options.rpcUrl !== "string" || !options.rpcUrl)
    throw new BaseError("invalid_configuration");
  const address = getAddress(options.address);
  const network = freezeBaseNetwork({ ...options.network, rpcUrl: options.rpcUrl });
  const transfers = createBaseTransfers(network);
  const reviews = new WeakSet<BasePaymentReview>();
  const rpc = (signal?: AbortSignal) =>
    createPublicClient({
      chain: network.chain,
      transport: http(network.rpcUrl, { retryCount: 0, timeout: 15_000, fetchOptions: { signal } }),
    });
  const check = (guard: BaseSessionGuard, review?: BasePaymentReview) => {
    if (guard.signal?.aborted) throw new BaseError("cancelled");
    try {
      guard.assertCurrent();
    } catch (cause) {
      throw new BaseError("wallet_changed", cause);
    }
    if (review && Date.now() >= review.deadline) throw new BaseError("review_expired");
  };
  const prepare = (
    asset: "eth" | "usdc",
    recipient: Address,
    amount: bigint,
    guard: BaseSessionGuard,
  ) =>
    (asset === "eth" ? transfers.prepareEth : transfers.prepareUsdc)(address, recipient, amount, {
      signal: guard.signal,
      stillCurrent: () => {
        check(guard);
        return true;
      },
    });
  return Object.freeze({
    address,
    network,
    getBalance: (guard: BaseSessionGuard): Promise<BaseBalance> =>
      baseOperation(async () => {
        check(guard);
        const client = rpc(guard.signal);
        if ((await client.getChainId()) !== network.chain.id) throw new BaseError("chain_mismatch");
        check(guard);
        const [eth, usdc] = await Promise.all([
          client.getBalance({ address }),
          client.readContract({
            address: network.usdc,
            abi: balanceAbi,
            functionName: "balanceOf",
            args: [address],
          }),
        ]);
        check(guard);
        if (
          typeof eth !== "bigint" ||
          typeof usdc !== "bigint" ||
          eth < 0n ||
          usdc < 0n ||
          eth >= 1n << 256n ||
          usdc >= 1n << 256n
        )
          throw new BaseError("invalid_response");
        return { eth, usdc };
      }),
    reviewPayment: (
      input: { asset: "eth" | "usdc"; recipient: Address; amount: bigint; deadline: number },
      guard: BaseSessionGuard,
    ): Promise<BasePaymentReview> =>
      baseOperation(async () => {
        const { asset, recipient, amount, deadline } = input;
        if (
          !["eth", "usdc"].includes(asset) ||
          !Number.isSafeInteger(deadline) ||
          deadline <= Date.now()
        )
          throw new BaseError("review_expired");
        check(guard);
        const prepared = await prepare(asset, recipient, amount, guard);
        const review = Object.freeze({
          asset,
          owner: address,
          recipient: getAddress(recipient),
          amount,
          chainId: network.chain.id,
          fee: prepared.fee,
          l2Max: prepared.l2Max,
          deadline,
        });
        check(guard, review);
        reviews.add(review);
        return review;
      }),
    sendPayment: (review: BasePaymentReview, send: BaseSendOptions): Promise<Hash> =>
      baseOperation(async () => {
        const { guard } = send;
        let attempted: BaseSubmission | undefined;
        if (!reviews.has(review)) throw new BaseError("review_invalid");
        check(guard, review);
        return baseOperation(
          () =>
            send.withSpendLock(async () => {
              check(guard, review);
              if (!reviews.has(review)) throw new BaseError("review_invalid");
              await baseOperation(async () => send.assertNoPending(), "payment_blocked");
              check(guard, review);
              const prepared = await prepare(review.asset, review.recipient, review.amount, guard);
              check(guard, review);
              if (prepared.fee > review.fee || prepared.l2Max > review.l2Max)
                throw new BaseError("fee_changed");
              return baseOperation(
                () =>
                  send.withSigner(async (account) => {
                    check(guard, review);
                    if (getAddress(account.address) !== address)
                      throw new BaseError("wallet_changed");
                    // Consume before invoking the signer. No signing retry after an ambiguous failure.
                    if (!reviews.has(review)) throw new BaseError("review_invalid");
                    reviews.delete(review);
                    const signed = await baseOperation(
                      () => account.signTransaction(prepared.transaction),
                      "signing_failed",
                    );
                    check(guard, review);
                    if (!signed.startsWith("0x02")) throw new BaseError("transaction_mismatch");
                    const serialized = signed as `0x02${string}`;
                    const parsed = await baseOperation(
                      async () => parseTransaction(serialized),
                      "transaction_mismatch",
                    );
                    const expected = prepared.transaction;
                    if (
                      parsed.type !== "eip1559" ||
                      parsed.chainId !== expected.chainId ||
                      !parsed.to ||
                      getAddress(parsed.to) !== getAddress(expected.to) ||
                      (parsed.value ?? 0n) !== expected.value ||
                      (parsed.data ?? "0x") !== ("data" in expected ? expected.data : "0x") ||
                      parsed.nonce !== expected.nonce ||
                      parsed.gas !== expected.gas ||
                      parsed.maxFeePerGas !== expected.maxFeePerGas ||
                      parsed.maxPriorityFeePerGas !== expected.maxPriorityFeePerGas ||
                      (parsed.accessList?.length ?? 0) !== 0 ||
                      getAddress(
                        await baseOperation(
                          () => recoverTransactionAddress({ serializedTransaction: serialized }),
                          "transaction_mismatch",
                        ),
                      ) !== address
                    )
                      throw new BaseError("transaction_mismatch");
                    check(guard, review);
                    const submission = Object.freeze({
                      ...(await baseTransactionRecord(signed)),
                      review,
                    });
                    check(guard, review);
                    try {
                      await send.reserveSubmission(submission);
                      check(guard, review);
                    } catch (cause) {
                      // No transport call was made. The application removes ONLY this hash
                      // inside the same spend lock; a failed release still fails closed.
                      await baseOperation(
                        () => send.releaseSubmission(submission),
                        "storage_failed",
                      );
                      throw new BaseSubmissionNotSentError(submission, cause);
                    }
                    // Do not abort this request with the UI session signal or retry it after submission.
                    try {
                      attempted = submission;
                      const result = await rpc().sendRawTransaction({
                        serializedTransaction: signed,
                      });
                      if (!isHash(result) || result.toLowerCase() !== submission.hash.toLowerCase())
                        throw new Error("RPC hash mismatch.");
                      return submission.hash;
                    } catch {
                      throw new BaseSubmissionUnknownError(submission);
                    }
                  }),
                "unlock_failed",
              );
            }),
          "payment_blocked",
        ).catch((error) => {
          // Application wrappers can throw or replace errors after their
          // callback returns. A transport attempt still owns this receipt.
          if (attempted && !(error instanceof BaseSubmissionUnknownError))
            throw new BaseSubmissionUnknownError(attempted);
          throw error;
        });
      }),
  });
}
export type BaseWallet = ReturnType<typeof createBaseWallet>;
