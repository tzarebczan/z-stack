import { BaseError } from "./errors";
import { getAddress, isAddress, zeroAddress, type Address, type Chain } from "viem";
import { base, baseSepolia } from "viem/chains";

// Clone only library-owned chain definitions; never freeze viem's shared globals.
function snapshot<T>(value: T): T {
  if (Array.isArray(value)) return Object.freeze(value.map(snapshot)) as T;
  if (value && typeof value === "object")
    return Object.freeze(
      Object.fromEntries(Object.entries(value).map(([key, child]) => [key, snapshot(child)])),
    ) as T;
  return value;
}

export const BASE_MAINNET = Object.freeze({
  chain: snapshot(base),
  usdc: "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913" as Address,
});
export const BASE_SEPOLIA = Object.freeze({
  chain: snapshot(baseSepolia),
  usdc: "0x036CbD53842c5426634e7929541eC2318f3dCF7e" as Address,
});
export interface BaseNetwork {
  readonly chain: Chain;
  readonly usdc: Address;
  /** Explicit provider choice. Never put a privileged API key here. */
  readonly rpcUrl: string;
}

export type BaseChain = Omit<BaseNetwork, "rpcUrl">;

export function freezeBaseChain(input: BaseChain): Readonly<BaseChain> {
  const chain = input.chain.id === 8453 ? base : input.chain.id === 84532 ? baseSepolia : undefined;
  if (!chain || !isAddress(input.usdc) || getAddress(input.usdc) === zeroAddress)
    throw new Error("Choose Base or Base Sepolia and an explicit USDC contract.");
  // Use the built-in chain definition; caller mutations cannot change the signing chain.
  return Object.freeze({
    chain: snapshot({ ...chain, id: input.chain.id }),
    usdc: getAddress(input.usdc),
  });
}

export function freezeBaseNetwork(input: BaseNetwork): Readonly<BaseNetwork> {
  if (typeof input.rpcUrl !== "string" || !input.rpcUrl)
    throw new BaseError("invalid_configuration");
  let url: URL;
  try {
    url = new URL(input.rpcUrl);
  } catch (cause) {
    throw new BaseError("invalid_configuration", cause);
  }
  if (
    url.username ||
    url.password ||
    url.hash ||
    (url.protocol !== "https:" &&
      !(url.protocol === "http:" && ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname)))
  )
    throw new BaseError("invalid_configuration");
  return Object.freeze({ ...freezeBaseChain(input), rpcUrl: input.rpcUrl });
}
