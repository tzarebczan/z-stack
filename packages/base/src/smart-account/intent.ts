import type { SmartAccountDeployment } from "./deployment";

import {
  decodeFunctionData,
  encodeFunctionData,
  getAddress,
  isAddress,
  zeroAddress,
  type Address,
  type Hex,
} from "viem";

import { accountAbi, usdcTransferAbi } from "./abis";

export interface SponsoredTransferIntent {
  readonly owner: Address;
  readonly recipient: Address;
  readonly amount: bigint;
  /** Local approval validity only. EIP-7702 authorizations have no expiry. */
  readonly deadline: number;
  readonly callData: Hex;
}
export function createIntents(manifest: Readonly<SmartAccountDeployment>) {
  function freezeTransferIntent(parameters: {
    owner: Address;
    recipient: Address;
    amount: bigint;
    deadline: number;
  }): Readonly<SponsoredTransferIntent> {
    if (
      !isAddress(parameters.owner) ||
      !isAddress(parameters.recipient) ||
      getAddress(parameters.recipient) === zeroAddress
    )
      throw new Error("Review a valid Base USDC recipient.");
    if (
      typeof parameters.amount !== "bigint" ||
      parameters.amount <= 0n ||
      parameters.amount >= 1n << 256n
    )
      throw new Error("Review a positive Base USDC amount.");
    if (!Number.isSafeInteger(parameters.deadline) || parameters.deadline <= 0)
      throw new Error("Invalid transfer review deadline.");
    const transfer = encodeFunctionData({
      abi: usdcTransferAbi,
      functionName: "transfer",
      args: [parameters.recipient, parameters.amount],
    });
    const callData = encodeFunctionData({
      abi: accountAbi,
      functionName: "execute",
      args: [manifest.usdc, 0n, transfer],
    });
    return Object.freeze({
      owner: getAddress(parameters.owner),
      recipient: getAddress(parameters.recipient),
      amount: parameters.amount,
      deadline: parameters.deadline,
      callData,
    });
  }

  /** Reject batches, approval grants, native transfers, altered amounts and noncanonical ABI bytes. */
  function assertTransferIntent(intent: SponsoredTransferIntent, callData: Hex) {
    const canonical = freezeTransferIntent(intent);
    const execution = decodeFunctionData({ abi: accountAbi, data: callData });
    if (
      execution.functionName !== "execute" ||
      getAddress(execution.args[0]) !== getAddress(manifest.usdc) ||
      execution.args[1] !== 0n
    )
      throw new Error("The sponsored operation is not the reviewed USDC transfer.");
    const transfer = decodeFunctionData({ abi: usdcTransferAbi, data: execution.args[2] });
    if (
      transfer.functionName !== "transfer" ||
      getAddress(transfer.args[0]) !== canonical.recipient ||
      transfer.args[1] !== canonical.amount ||
      callData.toLowerCase() !== canonical.callData.toLowerCase() ||
      intent.callData.toLowerCase() !== canonical.callData.toLowerCase()
    )
      throw new Error("The sponsored operation changed the reviewed transfer.");
  }
  return { freezeTransferIntent, assertTransferIntent };
}
