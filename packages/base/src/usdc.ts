import {
  createPublicClient,
  encodeFunctionData,
  http,
  isAddress,
  isHash,
  parseTransaction,
  type Hash,
  type Address,
  type HDAccount,
} from "viem";

import { BaseError } from "./errors";
import {
  baseTransactionRecord,
  BaseTransactionNotSentError,
  type BaseTransactionRecord,
} from "./transaction";
import { publicActionsL2 } from "viem/op-stack";

import type { BaseNetwork } from "./network";

import { estimateBaseOperatorFee } from "./operator-fee";

/** A lower-level broadcast was attempted; reconcile this locally derived hash, never retry the review. */
export class BaseTransferSubmissionUnknownError extends BaseError {
  constructor(
    readonly hash: Hash,
    readonly chainId: number,
  ) {
    super("submission_unknown");
    this.name = "BaseTransferSubmissionUnknownError";
  }
}

export interface PreparedUsdcTransfer {
  address: Address;
  fee: bigint;
  l2Max: bigint;
  variableFee: bigint;
  transaction: {
    chainId: number;
    type: "eip1559";
    to: Address;
    data: `0x${string}`;
    value: bigint;
    nonce: number;
    gas: bigint;
    maxFeePerGas: bigint;
    maxPriorityFeePerGas: bigint;
  };
}
export function createUsdcTransfers(network: Readonly<BaseNetwork>) {
  const ABI = [
    {
      name: "balanceOf",
      type: "function",
      stateMutability: "view",
      inputs: [{ name: "account", type: "address" }],
      outputs: [{ name: "", type: "uint256" }],
    },
    {
      name: "transfer",
      type: "function",
      stateMutability: "nonpayable",
      inputs: [
        { name: "to", type: "address" },
        { name: "amount", type: "uint256" },
      ],
      outputs: [{ name: "", type: "bool" }],
    },
  ] as const;

  const client = (signal?: AbortSignal) =>
    createPublicClient({
      chain: network.chain,
      transport: http(network.rpcUrl, { retryCount: 0, timeout: 15_000, fetchOptions: { signal } }),
    }).extend(publicActionsL2());

  const UINT256 = 1n << 256n;
  const uint = (value: unknown, positive = false): value is bigint =>
    typeof value === "bigint" && value >= (positive ? 1n : 0n) && value < UINT256;
  const prepared = new WeakSet<PreparedUsdcTransfer>();

  /** Exact USDC transfer, with fresh balances and OP Stack L1/L2/operator fee estimate. */
  async function prepareUsdcTransfer(
    address: string,
    to: string,
    amount: bigint,
    options: { signal?: AbortSignal; stillCurrent?: () => boolean } = {},
  ): Promise<PreparedUsdcTransfer> {
    const assertCurrent = () => {
      if (options.signal?.aborted || options.stillCurrent?.() === false)
        throw new BaseError("wallet_changed");
    };
    assertCurrent();
    if (!isAddress(address) || !isAddress(to) || /^0x0{40}$/i.test(to) || !uint(amount, true))
      throw new BaseError("invalid_payment");
    const rpc = client(options.signal);
    if ((await rpc.getChainId()) !== network.chain.id) throw new BaseError("chain_mismatch");
    assertCurrent();
    const from = address as Address;
    const data = encodeFunctionData({
      abi: ABI,
      functionName: "transfer",
      args: [to as Address, amount],
    });
    const [eth, usdc, gas, fees, nonce] = await Promise.all([
      rpc.getBalance({ address: from }),
      rpc.readContract({
        address: network.usdc,
        abi: ABI,
        functionName: "balanceOf",
        args: [from],
      }),
      rpc.estimateGas({ account: from, to: network.usdc, data, value: 0n }),
      rpc.estimateFeesPerGas(),
      rpc.getTransactionCount({ address: from, blockTag: "pending" }),
    ]);
    assertCurrent();
    if (
      !uint(eth) ||
      !uint(usdc) ||
      !uint(gas, true) ||
      gas >= 1n << 64n ||
      !uint(fees.maxFeePerGas, true) ||
      !uint(fees.maxPriorityFeePerGas) ||
      fees.maxPriorityFeePerGas > fees.maxFeePerGas ||
      !Number.isSafeInteger(nonce) ||
      nonce < 0
    )
      throw new BaseError("invalid_response");
    if (usdc < amount) throw new BaseError("insufficient_funds");
    const transaction = {
      chainId: network.chain.id,
      type: "eip1559" as const,
      to: network.usdc,
      data,
      value: 0n,
      nonce,
      gas: (gas * 120n + 99n) / 100n,
      maxFeePerGas: fees.maxFeePerGas,
      maxPriorityFeePerGas: fees.maxPriorityFeePerGas,
    };
    if (transaction.gas >= 1n << 64n) throw new BaseError("invalid_response");
    const [l1Fee, operatorFee] = await Promise.all([
      rpc.estimateL1Fee({ account: from, ...transaction }),
      estimateBaseOperatorFee(rpc, transaction.gas),
    ]);
    assertCurrent();
    const l2Max = transaction.gas * transaction.maxFeePerGas;
    if (!uint(l1Fee) || !uint(operatorFee) || !uint(l2Max, true))
      throw new BaseError("invalid_response");
    const variableFee = l1Fee + operatorFee;
    const fee = l2Max + variableFee;
    if (!uint(fee, true)) throw new BaseError("invalid_response");
    if (eth < fee) throw new BaseError("insufficient_funds");
    const result = { address: from, fee, l2Max, variableFee, transaction };
    Object.freeze(transaction);
    Object.freeze(result);
    prepared.add(result);
    return result;
  }

  /** One locally signed transaction. Transport retries are disabled; an unknown acknowledgement stays unknown. */
  async function submitUsdcTransfer(
    account: HDAccount,
    transfer: PreparedUsdcTransfer,
    stillCurrent: () => boolean,
    beforeBroadcast: (transaction: BaseTransactionRecord) => Promise<void>,
  ): Promise<string> {
    if (!prepared.has(transfer) || account.address.toLowerCase() !== transfer.address.toLowerCase())
      throw new BaseError("wallet_changed");
    if (!stillCurrent()) throw new BaseError("wallet_changed");
    prepared.delete(transfer);
    const signed = await account.signTransaction(transfer.transaction);
    if (!stillCurrent()) throw new BaseError("wallet_changed");
    const record = await baseTransactionRecord(signed);
    if (!stillCurrent()) throw new BaseError("wallet_changed");
    if (
      record.from.toLowerCase() !== transfer.address.toLowerCase() ||
      record.chainId !== transfer.transaction.chainId
    )
      throw new BaseError("transaction_mismatch");
    const parsed = parseTransaction(signed as `0x02${string}`),
      expected = transfer.transaction;
    if (
      parsed.to?.toLowerCase() !== expected.to.toLowerCase() ||
      (parsed.value ?? 0n) !== expected.value ||
      parsed.data !== expected.data ||
      parsed.nonce !== expected.nonce ||
      parsed.gas !== expected.gas ||
      parsed.maxFeePerGas !== expected.maxFeePerGas ||
      parsed.maxPriorityFeePerGas !== expected.maxPriorityFeePerGas ||
      (parsed.accessList?.length ?? 0) !== 0
    )
      throw new BaseError("transaction_mismatch");
    try {
      await beforeBroadcast(record);
      if (!stillCurrent()) throw new BaseError("wallet_changed");
    } catch (cause) {
      throw new BaseTransactionNotSentError(record, cause);
    }
    const hash = record.hash;
    try {
      const acknowledged = await client().sendRawTransaction({ serializedTransaction: signed });
      if (!isHash(acknowledged) || acknowledged.toLowerCase() !== hash.toLowerCase())
        throw new Error("RPC acknowledgement hash mismatch");
      return hash;
    } catch {
      throw new BaseTransferSubmissionUnknownError(hash, transfer.transaction.chainId);
    }
  }
  return { prepareUsdcTransfer, submitUsdcTransfer };
}
