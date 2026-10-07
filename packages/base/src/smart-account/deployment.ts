import { entryPoint08Address } from "viem/account-abstraction";
import { getAddress, isAddress, isHash, zeroAddress, type Address, type Hash } from "viem";

/** Only the Simple7702 account ABI and EntryPoint 0.8 signing scheme are supported. */
export interface SmartAccountDeployment {
  readonly chainId: 8453 | 84532;
  readonly delegate: Address;
  readonly delegateCodeHash: Hash;
  readonly entryPoint: Address;
  readonly entryPointCodeHash: Hash;
  readonly entryPointVersion: "0.8";
  readonly usdc: Address;
}
export function freezeSmartAccountDeployment(
  input: SmartAccountDeployment,
): Readonly<SmartAccountDeployment> {
  if (
    ![8453, 84532].includes(input.chainId) ||
    input.entryPointVersion !== "0.8" ||
    !isHash(input.delegateCodeHash) ||
    !isHash(input.entryPointCodeHash)
  )
    throw new Error(
      "Provide a verified Base Simple7702 deployment and EntryPoint 0.8 code hashes.",
    );
  for (const address of [input.delegate, input.entryPoint, input.usdc])
    if (!isAddress(address) || getAddress(address) === zeroAddress)
      throw new Error("Invalid Base smart account deployment address.");
  if (getAddress(input.entryPoint) !== getAddress(entryPoint08Address))
    throw new Error("Simple7702 requires the canonical EntryPoint 0.8 address.");
  return Object.freeze({
    chainId: input.chainId,
    delegate: getAddress(input.delegate),
    delegateCodeHash: input.delegateCodeHash,
    entryPoint: getAddress(input.entryPoint),
    entryPointCodeHash: input.entryPointCodeHash,
    entryPointVersion: "0.8",
    usdc: getAddress(input.usdc),
  });
}
