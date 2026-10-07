import type { BasePublicClient } from "./client";
import type { SmartAccountDeployment } from "./deployment";

import {
  decodeEventLog,
  getAddress,
  isHash,
  parseAbi,
  type Address,
  type Hash,
  type TransactionReceipt,
} from "viem";

import {} from "./abis";

import type { SponsoredTransferIntent } from "./intent";

export interface ExpectedSponsoredSettlement {
  readonly hash: Hash;
  readonly nonce: bigint;
  readonly paymaster: Address;
  readonly intent: SponsoredTransferIntent;
  readonly maxSponsoredCost: bigint;
}

export type SponsoredSettlement =
  | { phase: "pending" }
  | { phase: "integrity-error" }
  | { phase: "failed"; transactionHash: Hash }
  | { phase: "confirmed"; transactionHash: Hash };
export function createSettlement(manifest: Readonly<SmartAccountDeployment>) {
  const events = parseAbi([
    "event BeforeExecution()",
    "event UserOperationEvent(bytes32 indexed userOpHash, address indexed sender, address indexed paymaster, uint256 nonce, bool success, uint256 actualGasCost, uint256 actualGasUsed)",
    "event Transfer(address indexed from, address indexed to, uint256 value)",
  ]);

  /** A bundle transaction can include several payments. Only logs between the
   * preceding EntryPoint execution boundary and this operation's event belong
   * to its execution. Bundler-provided success and its filtered logs are hints. */
  function inspectReceipt(
    receipt: TransactionReceipt,
    expected: ExpectedSponsoredSettlement,
  ): SponsoredSettlement {
    if (receipt.status !== "success" || !isHash(receipt.transactionHash))
      return { phase: "integrity-error" };
    const logs = [...receipt.logs].sort(
      (left, right) => (left.logIndex ?? -1) - (right.logIndex ?? -1),
    );
    const indexes = new Set<number>();
    let boundary: number | null = null;
    let sawExecution = false;
    let matched = 0;
    let result: SponsoredSettlement = { phase: "pending" };
    for (const log of logs) {
      if (
        log.removed ||
        log.logIndex === null ||
        !Number.isSafeInteger(log.logIndex) ||
        log.logIndex < 0 ||
        indexes.has(log.logIndex) ||
        log.transactionHash !== receipt.transactionHash ||
        log.blockHash !== receipt.blockHash ||
        log.blockNumber !== receipt.blockNumber
      )
        return { phase: "integrity-error" };
      indexes.add(log.logIndex);
      if (getAddress(log.address) !== getAddress(manifest.entryPoint)) continue;
      let event;
      try {
        event = decodeEventLog({ abi: events, data: log.data, topics: log.topics });
      } catch {
        continue;
      }
      if (event.eventName === "BeforeExecution") {
        if (sawExecution || boundary !== null) return { phase: "integrity-error" };
        sawExecution = true;
        boundary = log.logIndex;
        continue;
      }
      if (event.eventName !== "UserOperationEvent") continue;
      if (!sawExecution) return { phase: "integrity-error" };
      const previous = boundary;
      boundary = log.logIndex;
      if (event.args.userOpHash.toLowerCase() !== expected.hash.toLowerCase()) continue;
      matched += 1;
      if (
        previous === null ||
        event.args.nonce !== expected.nonce ||
        getAddress(event.args.sender) !== getAddress(expected.intent.owner) ||
        getAddress(event.args.paymaster) !== getAddress(expected.paymaster) ||
        event.args.actualGasCost < 0n ||
        event.args.actualGasCost > expected.maxSponsoredCost
      ) {
        result = { phase: "integrity-error" };
        continue;
      }
      let transfers = 0;
      let exact = false;
      for (const candidate of logs) {
        if (
          candidate.logIndex! <= previous ||
          candidate.logIndex! >= log.logIndex ||
          getAddress(candidate.address) !== getAddress(manifest.usdc)
        )
          continue;
        try {
          const transfer = decodeEventLog({
            abi: events,
            data: candidate.data,
            topics: candidate.topics,
          });
          if (
            transfer.eventName !== "Transfer" ||
            getAddress(transfer.args.from) !== getAddress(expected.intent.owner)
          )
            continue;
          transfers += 1;
          exact =
            getAddress(transfer.args.to) === getAddress(expected.intent.recipient) &&
            transfer.args.value === expected.intent.amount;
        } catch {
          /* An unrelated token event cannot establish payment completion. */
        }
      }
      if (!event.args.success) {
        result =
          transfers === 0
            ? { phase: "failed", transactionHash: receipt.transactionHash }
            : { phase: "integrity-error" };
        continue;
      }
      result =
        transfers === 1 && exact
          ? { phase: "confirmed", transactionHash: receipt.transactionHash }
          : { phase: "integrity-error" };
    }
    return matched === 1 ? result : { phase: "integrity-error" };
  }

  function inspectSponsoredSettlement(
    receipt: TransactionReceipt,
    expected: ExpectedSponsoredSettlement,
  ): SponsoredSettlement {
    try {
      return inspectReceipt(receipt, expected);
    } catch {
      return { phase: "integrity-error" };
    }
  }

  /** Read only. The saved operation hash identifies the payment; the candidate
   * transaction hash merely locates a bundle. Require finalized canonical Base
   * data independently of a managed bundler's acknowledgement. */
  async function verifySponsoredSettlement(
    client: BasePublicClient,
    expected: ExpectedSponsoredSettlement,
    transactionHash: Hash,
    options: { signal?: AbortSignal; stillCurrent?: () => boolean } = {},
  ): Promise<SponsoredSettlement> {
    const current = () => !options.signal?.aborted && options.stillCurrent?.() !== false;
    if (
      !isHash(expected.hash) ||
      !isHash(transactionHash) ||
      expected.nonce < 0n ||
      expected.nonce >= 1n << 64n ||
      expected.maxSponsoredCost <= 0n
    )
      return { phase: "integrity-error" };
    try {
      if (!current() || (await client.getChainId()) !== manifest.chainId)
        return { phase: "pending" };
      const [receipt, finalized] = await Promise.all([
        client.getTransactionReceipt({ hash: transactionHash }),
        client.getBlock({ blockTag: "finalized" }),
      ]);
      if (!current() || finalized.number === null || receipt.blockNumber > finalized.number)
        return { phase: "pending" };
      if (receipt.transactionHash.toLowerCase() !== transactionHash.toLowerCase())
        return { phase: "integrity-error" };
      const canonical = await client.getBlock({ blockNumber: receipt.blockNumber });
      if (
        !current() ||
        canonical.hash !== receipt.blockHash ||
        (await client.getChainId()) !== manifest.chainId ||
        !current()
      )
        return { phase: "pending" };
      return inspectSponsoredSettlement(receipt, expected);
    } catch {
      return { phase: "pending" };
    }
  }
  return { inspectSponsoredSettlement, verifySponsoredSettlement };
}
