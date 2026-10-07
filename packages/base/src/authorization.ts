import { getAddress, isAddress, zeroAddress, type Address, type Hex } from "viem";
import type { HDAccount } from "viem/accounts";
import { freezeBaseChain, type BaseChain } from "./network";

export interface TransferAuthorization {
  from: string;
  to: string;
  value: string;
  validAfter: string;
  validBefore: string;
  nonce: string;
  signature: string;
}
/** An EIP-3009 signature authorizes a transfer. It does not submit one or choose a relayer. */
export async function signUsdcAuthorization(parameters: {
  network: Readonly<BaseChain>;
  account: HDAccount;
  recipient: Address;
  amount: bigint;
  assertCurrent(): void;
}): Promise<TransferAuthorization> {
  const { account, recipient, amount, assertCurrent } = parameters;
  const network = freezeBaseChain(parameters.network);
  if (
    !isAddress(recipient) ||
    getAddress(recipient) === zeroAddress ||
    typeof amount !== "bigint" ||
    amount <= 0n ||
    amount >= 1n << 256n
  )
    throw new Error("Enter a valid Base recipient and amount.");
  assertCurrent();
  const nonce =
    `0x${Array.from(crypto.getRandomValues(new Uint8Array(32)), (b) => b.toString(16).padStart(2, "0")).join("")}` as Hex;
  const validBefore = BigInt(Math.floor(Date.now() / 1000) + 300);
  const signature = await account.signTypedData({
    domain: {
      name: "USD Coin",
      version: "2",
      chainId: network.chain.id,
      verifyingContract: network.usdc,
    },
    types: {
      TransferWithAuthorization: [
        { name: "from", type: "address" },
        { name: "to", type: "address" },
        { name: "value", type: "uint256" },
        { name: "validAfter", type: "uint256" },
        { name: "validBefore", type: "uint256" },
        { name: "nonce", type: "bytes32" },
      ],
    },
    primaryType: "TransferWithAuthorization",
    message: {
      from: account.address,
      to: recipient,
      value: amount,
      validAfter: 0n,
      validBefore,
      nonce,
    },
  });
  assertCurrent();
  if (BigInt(Math.floor(Date.now() / 1000)) >= validBefore)
    throw new Error("The payment authorization expired. Review it again.");
  return {
    from: account.address,
    to: recipient,
    value: amount.toString(),
    validAfter: "0",
    validBefore: validBefore.toString(),
    nonce,
    signature,
  };
}
