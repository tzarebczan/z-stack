import { BaseError } from "./errors";
import { createPublicClient, http, isAddress, type Address } from "viem";

import type { BaseNetwork } from "./network";

import { publicActionsL2 } from "viem/op-stack";

import { estimateBaseOperatorFee } from "./operator-fee";

export function createEthTransfers(network: Readonly<BaseNetwork>) {
  const UINT256 = 1n << 256n;

  function uint(value: unknown, positive = false): value is bigint {
    return typeof value === "bigint" && value >= (positive ? 1n : 0n) && value < UINT256;
  }

  /** Build exact native ETH bytes locally; RPC supplies only bounded estimates and nonce. */
  async function prepareEthTransfer(
    address: string,
    to: string,
    amount: bigint,
    options: { signal?: AbortSignal; stillCurrent?: () => boolean } = {},
  ) {
    const assertCurrent = () => {
      if (options.signal?.aborted || options.stillCurrent?.() === false)
        throw new BaseError("wallet_changed");
    };
    assertCurrent();
    if (!isAddress(address) || !isAddress(to) || /^0x0{40}$/i.test(to) || !uint(amount, true))
      throw new BaseError("invalid_payment");
    const rpc = createPublicClient({
      chain: network.chain,
      transport: http(network.rpcUrl, {
        retryCount: 0,
        timeout: 15_000,
        fetchOptions: { signal: options.signal },
      }),
    }).extend(publicActionsL2());
    if ((await rpc.getChainId()) !== network.chain.id) throw new BaseError("chain_mismatch");
    assertCurrent();
    const from = address as Address;
    const [eth, gas, fees, nonce] = await Promise.all([
      rpc.getBalance({ address: from }),
      rpc.estimateGas({ account: from, to: to as Address, value: amount }),
      rpc.estimateFeesPerGas(),
      rpc.getTransactionCount({ address: from, blockTag: "pending" }),
    ]);
    assertCurrent();
    if (
      !uint(eth) ||
      !uint(gas, true) ||
      gas >= 1n << 64n ||
      !uint(fees.maxFeePerGas, true) ||
      !uint(fees.maxPriorityFeePerGas) ||
      fees.maxPriorityFeePerGas > fees.maxFeePerGas ||
      !Number.isSafeInteger(nonce) ||
      nonce < 0
    )
      throw new BaseError("invalid_response");
    const transaction = Object.freeze({
      chainId: network.chain.id,
      type: "eip1559" as const,
      to: to as Address,
      value: amount,
      nonce,
      gas: (gas * 120n + 99n) / 100n,
      maxFeePerGas: fees.maxFeePerGas,
      maxPriorityFeePerGas: fees.maxPriorityFeePerGas,
    });
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
    if (!uint(fee, true) || amount + fee >= UINT256) throw new BaseError("invalid_response");
    if (eth < amount + fee) throw new BaseError("insufficient_funds");
    return Object.freeze({ fee, l2Max, variableFee, transaction });
  }
  return { prepareEthTransfer };
}
