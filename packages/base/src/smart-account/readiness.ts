import type { BasePublicClient } from "./client";
import type { SmartAccountDeployment } from "./deployment";

import { getAddress, keccak256, type Address, type Hash } from "viem";

import { accountAbi, nonceAbi } from "./abis";

export interface SmartAccountReadiness {
  readonly owner: Address;
  readonly blockNumber: bigint;
  readonly blockHash: Hash;
  readonly delegation: "empty" | "pinned";
  readonly authorizationNonce: number;
  readonly operationNonce: bigint;
}
export function createReadiness(manifest: Readonly<SmartAccountDeployment>) {
  const delegationCode = `0xef0100${manifest.delegate.slice(2).toLowerCase()}`;

  /** Restore the delegation from chain reads; do not rely on viem's cached isDeployed flag. */
  async function readSmartAccountReadiness(
    client: BasePublicClient,
    owner: Address,
  ): Promise<Readonly<SmartAccountReadiness>> {
    if ((await client.getChainId()) !== manifest.chainId)
      throw new Error("Base chain ID mismatch.");
    const canonical = await client.getBlock({ blockTag: "latest" });
    if (canonical.number === null || canonical.hash === null)
      throw new Error("Missing canonical Base block.");
    const blockNumber = canonical.number;
    const [
      delegate,
      entryPoint,
      ownerCode,
      configuredEntryPoint,
      operationNonce,
      authorizationNonce,
      pendingAuthorizationNonce,
    ] = await Promise.all([
      client.getCode({ address: manifest.delegate, blockNumber }),
      client.getCode({ address: manifest.entryPoint, blockNumber }),
      client.getCode({ address: owner, blockNumber }),
      client.readContract({
        address: manifest.delegate,
        abi: accountAbi,
        functionName: "entryPoint",
        blockNumber,
      }),
      client.readContract({
        address: manifest.entryPoint,
        abi: nonceAbi,
        functionName: "getNonce",
        args: [owner, 0n],
        blockNumber,
      }),
      client.getTransactionCount({ address: owner, blockNumber }),
      client.getTransactionCount({ address: owner, blockTag: "pending" }),
    ]);
    if (!delegate || keccak256(delegate) !== manifest.delegateCodeHash)
      throw new Error("Base account implementation code is unverified.");
    if (!entryPoint || keccak256(entryPoint) !== manifest.entryPointCodeHash)
      throw new Error("Base EntryPoint code is unverified.");
    if (getAddress(configuredEntryPoint) !== getAddress(manifest.entryPoint))
      throw new Error("Base account EntryPoint mismatch.");
    const delegation =
      !ownerCode || ownerCode === "0x"
        ? "empty"
        : ownerCode.toLowerCase() === delegationCode
          ? "pinned"
          : undefined;
    if (!delegation) throw new Error("The wallet has unknown Base account code.");
    if (!Number.isSafeInteger(authorizationNonce) || authorizationNonce < 0)
      throw new Error("Invalid Base authorization nonce.");
    if (
      !Number.isSafeInteger(pendingAuthorizationNonce) ||
      pendingAuthorizationNonce !== authorizationNonce
    )
      throw new Error(
        "A Base authorization nonce is pending. Wait for confirmation before signing.",
      );
    if (operationNonce < 0n || operationNonce >= 1n << 64n)
      throw new Error("Base account nonce must use the fixed zero lane.");
    const [confirmed, chainId] = await Promise.all([
      client.getBlock({ blockNumber }),
      client.getChainId(),
    ]);
    if (confirmed.hash !== canonical.hash || chainId !== manifest.chainId)
      throw new Error("Base canonical state changed while checking readiness.");
    return Object.freeze({
      owner: getAddress(owner),
      blockNumber,
      blockHash: canonical.hash,
      delegation,
      authorizationNonce,
      operationNonce,
    });
  }

  function assertReadinessUnchanged(
    expected: SmartAccountReadiness,
    current: SmartAccountReadiness,
  ) {
    if (
      getAddress(expected.owner) !== getAddress(current.owner) ||
      expected.delegation !== current.delegation ||
      expected.authorizationNonce !== current.authorizationNonce ||
      expected.operationNonce !== current.operationNonce
    )
      throw new Error("Base wallet state changed. Review the transfer again.");
  }
  return { readSmartAccountReadiness, assertReadinessUnchanged };
}
