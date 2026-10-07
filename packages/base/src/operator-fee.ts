import { BaseError } from "./errors";
const ORACLE = "0x420000000000000000000000000000000000000F" as const;
const ABI = [
  {
    name: "getOperatorFee",
    type: "function",
    stateMutability: "view",
    inputs: [{ name: "gasUsed", type: "uint256" }],
    outputs: [{ name: "fee", type: "uint256" }],
  },
] as const;
interface OracleReader {
  readContract(parameters: {
    address: typeof ORACLE;
    abi: typeof ABI;
    functionName: "getOperatorFee";
    args: readonly [bigint];
  }): Promise<unknown>;
}

/** The predeploy selects the active fork's formula. An unavailable read is never a zero fee. */
export async function estimateBaseOperatorFee(rpc: OracleReader, gas: bigint): Promise<bigint> {
  if (typeof gas !== "bigint" || gas <= 0n || gas >= 1n << 64n)
    throw new BaseError("invalid_response");
  let fee: unknown;
  try {
    fee = await rpc.readContract({
      address: ORACLE,
      abi: ABI,
      functionName: "getOperatorFee",
      args: [gas],
    });
  } catch (cause) {
    throw new BaseError("rpc_unavailable", cause);
  }
  if (typeof fee !== "bigint" || fee < 0n || fee >= 1n << 256n)
    throw new BaseError("invalid_response");
  return fee;
}
