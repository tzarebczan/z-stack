import {
  createPublicClient,
  decodeEventLog,
  decodeFunctionData,
  parseAbi,
  getAddress,
  http,
  isHash,
  isHex,
  keccak256,
  parseTransaction,
  recoverTransactionAddress,
  TransactionNotFoundError,
  TransactionReceiptNotFoundError,
  type Address,
  type Hash,
  type Hex,
} from "viem";
import { freezeBaseNetwork, type BaseNetwork } from "./network";
import { BaseError, baseOperation } from "./errors";
import type { BaseSessionGuard } from "./wallet";

/** Signed transaction authority. Keep private until broadcast; never store a seed here. */
export interface BaseTransactionRecord {
  readonly hash: Hash;
  readonly chainId: number;
  readonly from: Address;
  readonly nonce: number;
  readonly serializedTransaction: Hex;
}
export type BaseTransactionStatus =
  | "unknown"
  | "pending"
  | "included"
  | "confirmed"
  | "failed"
  | "nonce-consumed";
/** A durable save or final guard failed before any transport call. */
export class BaseTransactionNotSentError extends BaseError {
  constructor(
    readonly transaction: BaseTransactionRecord,
    cause?: unknown,
  ) {
    super("submission_not_sent", cause);
    this.name = "BaseTransactionNotSentError";
    Object.defineProperty(this, "transaction", { value: transaction, enumerable: false });
  }
}
export function assertBaseSession(guard: BaseSessionGuard): void {
  if (guard.signal?.aborted) throw new BaseError("cancelled");
  try {
    guard.assertCurrent();
  } catch (cause) {
    throw new BaseError("wallet_changed", cause);
  }
}
export async function baseTransactionRecord(
  serializedTransaction: Hex,
): Promise<BaseTransactionRecord> {
  return baseOperation(async () => {
    if (
      typeof serializedTransaction !== "string" ||
      serializedTransaction.length > 131_072 ||
      !isHex(serializedTransaction, { strict: true }) ||
      !serializedTransaction.startsWith("0x02")
    )
      throw new BaseError("transaction_mismatch");
    const parsed = parseTransaction(serializedTransaction);
    if (
      !Number.isSafeInteger(parsed.nonce) ||
      parsed.nonce! < 0 ||
      ![8453, 84532].includes(parsed.chainId!)
    )
      throw new BaseError("transaction_mismatch");
    return Object.freeze({
      hash: keccak256(serializedTransaction),
      chainId: parsed.chainId!,
      from: getAddress(
        await recoverTransactionAddress({
          serializedTransaction: serializedTransaction as `0x02${string}`,
        }),
      ),
      nonce: parsed.nonce!,
      serializedTransaction,
    });
  }, "transaction_mismatch");
}
async function checked(
  network: BaseNetwork,
  input: BaseTransactionRecord,
  guard: BaseSessionGuard,
) {
  assertBaseSession(guard);
  const record = await baseTransactionRecord(input.serializedTransaction);
  assertBaseSession(guard);
  if (
    !isHash(input.hash) ||
    record.hash.toLowerCase() !== input.hash.toLowerCase() ||
    record.chainId !== network.chain.id ||
    record.chainId !== input.chainId ||
    record.nonce !== input.nonce ||
    record.from.toLowerCase() !== input.from?.toLowerCase()
  )
    throw new BaseError("transaction_mismatch");
  return record;
}

/** Read only. Missing receipts stay uncertain; consumed nonce does not prove non-payment. */
interface TransactionStatusOptions {
  network: BaseNetwork;
  transaction: BaseTransactionRecord;
  guard: BaseSessionGuard;
}
export async function getBaseTransactionStatus(
  options: TransactionStatusOptions,
): Promise<BaseTransactionStatus> {
  return transactionStatus(options, false);
}
/** Final payment proof for plain ETH or USDC transfer calldata, including the exact token Transfer event. */
export async function getBasePaymentStatus(
  options: TransactionStatusOptions,
): Promise<BaseTransactionStatus> {
  return transactionStatus(options, true);
}
async function transactionStatus(
  options: {
    network: BaseNetwork;
    transaction: BaseTransactionRecord;
    guard: BaseSessionGuard;
  },
  verifyPayment: boolean,
): Promise<BaseTransactionStatus> {
  return baseOperation(async () => {
    const network = freezeBaseNetwork(options.network),
      guard = options.guard;
    const record = await checked(network, options.transaction, guard);
    const rpc = createPublicClient({
      chain: network.chain,
      transport: http(network.rpcUrl, {
        retryCount: 0,
        timeout: 15_000,
        fetchOptions: { signal: guard.signal },
      }),
    });
    if ((await rpc.getChainId()) !== record.chainId) throw new BaseError("chain_mismatch");
    assertBaseSession(guard);
    let receipt;
    try {
      receipt = await rpc.getTransactionReceipt({ hash: record.hash });
    } catch (error) {
      if (!(error instanceof TransactionReceiptNotFoundError)) throw error;
    }
    assertBaseSession(guard);
    if (receipt) {
      const finalized = await rpc.getBlock({ blockTag: "finalized" });
      assertBaseSession(guard);
      if (receipt.transactionHash.toLowerCase() !== record.hash.toLowerCase())
        throw new BaseError("verification_failed");
      if (finalized.number === null || receipt.blockNumber > finalized.number) return "included";
      const canonical = await rpc.getBlock({ blockNumber: receipt.blockNumber });
      assertBaseSession(guard);
      if (
        receipt.transactionHash.toLowerCase() !== record.hash.toLowerCase() ||
        canonical.hash !== receipt.blockHash
      )
        throw new BaseError("verification_failed");
      if (!["success", "reverted"].includes(receipt.status))
        throw new BaseError("verification_failed");
      if (verifyPayment && receipt.status === "success") {
        const tx = parseTransaction(record.serializedTransaction as `0x02${string}`);
        if (tx.data && tx.data !== "0x") {
          if (tx.to?.toLowerCase() !== network.usdc.toLowerCase() || (tx.value ?? 0n) !== 0n)
            throw new BaseError("verification_failed");
          const data = tx.data;
          const transfer = await baseOperation(
            async () =>
              decodeFunctionData({
                abi: parseAbi(["function transfer(address to, uint256 amount) returns (bool)"]),
                data,
              }),
            "verification_failed",
          );
          const matched = receipt.logs?.some((log) => {
            if (
              log.removed ||
              log.address.toLowerCase() !== network.usdc.toLowerCase() ||
              log.transactionHash?.toLowerCase() !== record.hash.toLowerCase() ||
              log.blockHash !== receipt.blockHash
            )
              return false;
            try {
              const event = decodeEventLog({
                abi: parseAbi([
                  "event Transfer(address indexed from, address indexed to, uint256 value)",
                ]),
                data: log.data,
                topics: log.topics,
                strict: true,
              });
              return (
                event.args.from.toLowerCase() === record.from.toLowerCase() &&
                event.args.to.toLowerCase() === transfer.args[0].toLowerCase() &&
                event.args.value === transfer.args[1]
              );
            } catch {
              return false;
            }
          });
          if (!matched) throw new BaseError("verification_failed");
        } else if (!tx.to) throw new BaseError("verification_failed");
      }
      return receipt.status === "success" ? "confirmed" : "failed";
    }
    let transaction;
    try {
      transaction = await rpc.getTransaction({ hash: record.hash });
    } catch (error) {
      if (!(error instanceof TransactionNotFoundError)) throw error;
    }
    assertBaseSession(guard);
    if (transaction) {
      if (
        transaction.hash.toLowerCase() !== record.hash.toLowerCase() ||
        transaction.from.toLowerCase() !== record.from.toLowerCase() ||
        transaction.nonce !== record.nonce ||
        transaction.chainId !== record.chainId
      )
        throw new BaseError("verification_failed");
      return transaction.blockNumber == null ? "pending" : "included";
    }
    const nonce = await rpc.getTransactionCount({ address: record.from, blockTag: "finalized" });
    assertBaseSession(guard);
    if (!Number.isSafeInteger(nonce) || nonce < 0) throw new BaseError("verification_failed");
    return nonce > record.nonce ? "nonce-consumed" : "unknown";
  });
}

/** One explicit retry of the IDENTICAL signed bytes. Caller must hold its spend lock and match its saved record. */
export async function rebroadcastBaseTransaction(options: {
  network: BaseNetwork;
  transaction: BaseTransactionRecord;
  guard: BaseSessionGuard;
}): Promise<Hash> {
  return baseOperation(async () => {
    const network = freezeBaseNetwork(options.network),
      guard = options.guard;
    const record = await checked(network, options.transaction, guard);
    const rpc = createPublicClient({
      chain: network.chain,
      transport: http(network.rpcUrl, { retryCount: 0, timeout: 15_000 }),
    });
    if ((await rpc.getChainId()) !== record.chainId) throw new BaseError("chain_mismatch");
    assertBaseSession(guard);
    try {
      const hash = await rpc.sendRawTransaction({
        serializedTransaction: record.serializedTransaction,
      });
      if (!isHash(hash) || hash.toLowerCase() !== record.hash.toLowerCase()) throw new Error();
      return record.hash;
    } catch (cause) {
      throw new BaseError("submission_unknown", cause);
    }
  });
}
