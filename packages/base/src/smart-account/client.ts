import type { PublicClient } from "viem";

/** Application-owned viem public client. No transport, credentials or polling is selected here. */
export interface BasePublicClient extends PublicClient {
  /** Read contract code at the specified canonical block. */
  getCode: PublicClient["getCode"];
  /** Standard viem action; the Base wallet does not call block simulation. */
  simulateBlocks: PublicClient["simulateBlocks"];
}
