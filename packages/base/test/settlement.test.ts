import { beforeEach, expect, test, vi } from "vitest";
import {
  encodeAbiParameters,
  encodeEventTopics,
  parseAbi,
  type Address,
  type Hash,
  type PublicClient,
  type TransactionReceipt,
} from "viem";
import { freezeTransferIntent } from "./test-bindings";
import { BASE_SMART_ACCOUNT as manifest } from "./test-bindings";
import {
  inspectSponsoredSettlement,
  verifySponsoredSettlement,
  type ExpectedSponsoredSettlement,
} from "./test-bindings";

const owner = "0x1111111111111111111111111111111111111111";
const recipient = "0x2222222222222222222222222222222222222222";
const paymaster = "0x3333333333333333333333333333333333333333";
const operationHash = `0x${"ab".repeat(32)}` as Hash;
const transactionHash = `0x${"cd".repeat(32)}` as Hash;
const blockHash = `0x${"ef".repeat(32)}` as Hash;
const abi = parseAbi([
  "event BeforeExecution()",
  "event UserOperationEvent(bytes32 indexed userOpHash, address indexed sender, address indexed paymaster, uint256 nonce, bool success, uint256 actualGasCost, uint256 actualGasUsed)",
  "event Transfer(address indexed from, address indexed to, uint256 value)",
]);
const expected: ExpectedSponsoredSettlement = {
  hash: operationHash,
  nonce: 7n,
  paymaster,
  maxSponsoredCost: 1_000n,
  intent: freezeTransferIntent({ owner, recipient, amount: 123n, deadline: 1_000 }),
};
function log(index: number, address: string, topics: unknown, data: string) {
  return {
    logIndex: index,
    address,
    topics,
    data,
    removed: false,
    transactionHash,
    blockHash,
    blockNumber: 100n,
    transactionIndex: 0,
  };
}
function boundary(index: number) {
  return log(
    index,
    manifest.entryPoint,
    encodeEventTopics({ abi, eventName: "BeforeExecution" }),
    "0x",
  );
}
function transfer(index: number, amount = 123n, to: Address = recipient) {
  return log(
    index,
    manifest.usdc,
    encodeEventTopics({ abi, eventName: "Transfer", args: { from: owner, to } }),
    encodeAbiParameters([{ type: "uint256" }], [amount]),
  );
}
function operation(
  index: number,
  changes: {
    hash?: Hash;
    sender?: Address;
    nonce?: bigint;
    paymaster?: Address;
    success?: boolean;
    cost?: bigint;
  } = {},
) {
  return log(
    index,
    manifest.entryPoint,
    encodeEventTopics({
      abi,
      eventName: "UserOperationEvent",
      args: {
        userOpHash: changes.hash ?? operationHash,
        sender: changes.sender ?? owner,
        paymaster: changes.paymaster ?? paymaster,
      },
    }),
    encodeAbiParameters(
      [{ type: "uint256" }, { type: "bool" }, { type: "uint256" }, { type: "uint256" }],
      [changes.nonce ?? 7n, changes.success ?? true, changes.cost ?? 500n, 1n],
    ),
  );
}
function receipt(
  logs = [boundary(0), transfer(1), operation(2)],
  changes = {},
): TransactionReceipt {
  return {
    transactionHash,
    blockHash,
    blockNumber: 100n,
    status: "success",
    logs,
    ...changes,
  } as unknown as TransactionReceipt;
}
const fake = { chain: vi.fn(), receipt: vi.fn(), block: vi.fn() };
const client = {
  getChainId: fake.chain,
  getTransactionReceipt: fake.receipt,
  getBlock: fake.block,
} as unknown as PublicClient;
beforeEach(() => {
  vi.clearAllMocks();
  fake.chain.mockResolvedValue(manifest.chainId);
  fake.receipt.mockResolvedValue(receipt());
  fake.block.mockImplementation(async ({ blockTag }) =>
    blockTag === "finalized" ? { number: 200n } : { hash: blockHash },
  );
});

test("canonical finalized exact UserOp transfer confirms even after local review expiry", async () => {
  expect(await verifySponsoredSettlement(client, expected, transactionHash)).toEqual({
    phase: "confirmed",
    transactionHash,
  });
});
test("transfer from a preceding operation cannot establish this operation's payment", () => {
  expect(
    inspectSponsoredSettlement(
      receipt([
        boundary(0),
        transfer(1),
        operation(2, { hash: `0x${"11".repeat(32)}` }),
        operation(3),
      ]),
      expected,
    ).phase,
  ).toBe("integrity-error");
});
test("transfer from a later operation cannot establish this operation's payment", () => {
  expect(
    inspectSponsoredSettlement(
      receipt([
        boundary(0),
        operation(1),
        transfer(2),
        operation(3, { hash: `0x${"11".repeat(32)}` }),
      ]),
      expected,
    ).phase,
  ).toBe("integrity-error");
});
test("matching operation is correctly attributed among two successful bundle operations", () => {
  expect(
    inspectSponsoredSettlement(
      receipt([
        boundary(0),
        transfer(1, 9n),
        operation(2, { hash: `0x${"11".repeat(32)}` }),
        transfer(3),
        operation(4),
      ]),
      expected,
    ),
  ).toEqual({ phase: "confirmed", transactionHash });
});
for (const [name, changes] of Object.entries({
  sender: { sender: recipient as Address },
  nonce: { nonce: 8n },
  paymaster: { paymaster: recipient as Address },
  cost: { cost: 1_001n },
})) {
  test(`wrong ${name} cannot establish completion`, () => {
    expect(
      inspectSponsoredSettlement(
        receipt([boundary(0), transfer(1), operation(2, changes)]),
        expected,
      ).phase,
    ).toBe("integrity-error");
  });
}
test("an explicitly failed matching UserOp is terminal only with finalized canonical proof", async () => {
  fake.receipt.mockResolvedValue(receipt([boundary(0), operation(1, { success: false })]));
  expect(await verifySponsoredSettlement(client, expected, transactionHash)).toEqual({
    phase: "failed",
    transactionHash,
  });
});
test.each([
  [transfer(1, 124n), operation(2)],
  [transfer(1, 123n, owner), operation(2)],
  [transfer(1), transfer(2), operation(3)],
  [operation(1)],
])("missing, wrong or multiple transfers remain unresolved", (...logs) => {
  expect(inspectSponsoredSettlement(receipt([boundary(0), ...logs]), expected).phase).toBe(
    "integrity-error",
  );
});
test("a duplicate matching UserOp event is ambiguous", () => {
  expect(
    inspectSponsoredSettlement(
      receipt([boundary(0), transfer(1), operation(2), transfer(3), operation(4)]),
      expected,
    ).phase,
  ).toBe("integrity-error");
});
test("the execution boundary is required", () => {
  expect(inspectSponsoredSettlement(receipt([transfer(1), operation(2)]), expected).phase).toBe(
    "integrity-error",
  );
});
test.each(["removed", "duplicate-index", "foreign-block", "foreign-transaction"])(
  "%s logs cannot establish completion",
  (kind) => {
    const logs = [boundary(0), transfer(1), operation(2)];
    if (kind === "removed") logs[1]!.removed = true;
    if (kind === "duplicate-index") logs[1]!.logIndex = 0;
    if (kind === "foreign-block") logs[1]!.blockHash = `0x${"99".repeat(32)}`;
    if (kind === "foreign-transaction") logs[1]!.transactionHash = `0x${"99".repeat(32)}`;
    expect(inspectSponsoredSettlement(receipt(logs), expected).phase).toBe("integrity-error");
  },
);
test("reorg, nonfinality, RPC outage and chain mismatch retain pending state", async () => {
  fake.block.mockResolvedValue({ number: 99n, hash: blockHash });
  expect((await verifySponsoredSettlement(client, expected, transactionHash)).phase).toBe(
    "pending",
  );
  fake.block.mockImplementation(async ({ blockTag }) =>
    blockTag === "finalized" ? { number: 200n } : { hash: `0x${"99".repeat(32)}` },
  );
  expect((await verifySponsoredSettlement(client, expected, transactionHash)).phase).toBe(
    "pending",
  );
  fake.receipt.mockRejectedValue(new Error("RPC unavailable"));
  expect((await verifySponsoredSettlement(client, expected, transactionHash)).phase).toBe(
    "pending",
  );
  fake.chain.mockResolvedValue(1);
  expect((await verifySponsoredSettlement(client, expected, transactionHash)).phase).toBe(
    "pending",
  );
});
test("cancelled and stale ownership checks never publish completion", async () => {
  const controller = new AbortController();
  controller.abort();
  expect(
    (
      await verifySponsoredSettlement(client, expected, transactionHash, {
        signal: controller.signal,
      })
    ).phase,
  ).toBe("pending");
  expect(fake.receipt).not.toHaveBeenCalled();
  const lateController = new AbortController();
  fake.receipt.mockImplementation(async () => {
    lateController.abort();
    return receipt();
  });
  const current = () => !lateController.signal.aborted;
  expect(
    (await verifySponsoredSettlement(client, expected, transactionHash, { stillCurrent: current }))
      .phase,
  ).toBe("pending");
});

test("failed operation with a contradictory owner transfer remains unresolved", () => {
  expect(
    inspectSponsoredSettlement(
      receipt([boundary(0), transfer(1), operation(2, { success: false })]),
      expected,
    ).phase,
  ).toBe("integrity-error");
});
test("duplicate execution boundary cannot truncate the receipt attribution", () => {
  expect(
    inspectSponsoredSettlement(
      receipt([boundary(0), transfer(1), boundary(2), transfer(3), operation(4)]),
      expected,
    ).phase,
  ).toBe("integrity-error");
});
test("malformed receipt address and log index fail closed", () => {
  const malformed = transfer(1);
  malformed.address = "invalid";
  expect(
    inspectSponsoredSettlement(receipt([boundary(0), malformed, operation(2)]), expected).phase,
  ).toBe("integrity-error");
  malformed.address = manifest.usdc;
  malformed.logIndex = 1.5;
  expect(
    inspectSponsoredSettlement(receipt([boundary(0), malformed, operation(2)]), expected).phase,
  ).toBe("integrity-error");
});
