import { beforeEach, expect, test, vi } from "vitest";
const f = vi.hoisted(() => ({
  rpc: {
    getChainId: vi.fn(),
    getTransactionReceipt: vi.fn(),
    getTransaction: vi.fn(),
    getBlock: vi.fn(),
    getTransactionCount: vi.fn(),
    sendRawTransaction: vi.fn(),
  },
  client: vi.fn(),
  http: vi.fn(),
}));
vi.mock("viem", async (original) => ({
  ...(await original<typeof import("viem")>()),
  createPublicClient: f.client,
  http: f.http,
}));
import {
  TransactionNotFoundError,
  TransactionReceiptNotFoundError,
  encodeFunctionData,
  encodeEventTopics,
  encodeAbiParameters,
  parseAbi,
  keccak256,
} from "viem";
import {
  BASE_SEPOLIA,
  BaseError,
  baseErrorMessage,
  baseTransactionRecord,
  createBaseTransfers,
  createBaseWallet,
  deriveBaseAccount,
  getBaseTransactionStatus,
  getBasePaymentStatus,
  rebroadcastBaseTransaction,
} from "../src/index";
const signer = deriveBaseAccount("test test test test test test test test test test test junk");
const raw = await signer.signTransaction({
  chainId: 84532,
  type: "eip1559",
  to: "0x2222222222222222222222222222222222222222",
  value: 1n,
  nonce: 7,
  gas: 21000n,
  maxFeePerGas: 10n,
  maxPriorityFeePerGas: 1n,
});
const record = await baseTransactionRecord(raw);
const network = { ...BASE_SEPOLIA, rpcUrl: "http://localhost:8545" };
const guard = { assertCurrent: vi.fn() };
const options = () => ({ network, transaction: record, guard });
const blockHash = `0x${"aa".repeat(32)}`;
beforeEach(() => {
  vi.resetAllMocks();
  f.client.mockReturnValue(f.rpc);
  f.rpc.getChainId.mockResolvedValue(84532);
  f.rpc.getTransactionReceipt.mockRejectedValue(
    new TransactionReceiptNotFoundError({ hash: record.hash }),
  );
  f.rpc.getTransaction.mockRejectedValue(new TransactionNotFoundError({ hash: record.hash }));
  f.rpc.getTransactionCount.mockResolvedValue(7);
  f.rpc.getBlock.mockImplementation(async ({ blockTag }) =>
    blockTag === "finalized" ? { number: 100n } : { hash: blockHash },
  );
  f.rpc.sendRawTransaction.mockImplementation(async ({ serializedTransaction }) =>
    keccak256(serializedTransaction),
  );
});
test("record binds the real signature, chain, nonce and local hash without an RPC", () => {
  expect(record).toEqual({
    from: signer.address,
    chainId: 84532,
    nonce: 7,
    hash: keccak256(raw),
    serializedTransaction: raw,
  });
  expect(f.client).not.toHaveBeenCalled();
});
test.each(["hash", "from", "nonce", "chainId"] as const)(
  "edited %s record cannot read or rebroadcast",
  async (field) => {
    const transaction = {
      ...record,
      [field]:
        field === "hash"
          ? `0x${"bb".repeat(32)}`
          : field === "from"
            ? "0x2222222222222222222222222222222222222222"
            : 8,
    };
    await expect(rebroadcastBaseTransaction({ ...options(), transaction })).rejects.toMatchObject({
      code: "transaction_mismatch",
    });
    expect(f.client).not.toHaveBeenCalled();
  },
);
test("missing receipts remain unknown and a consumed finalized nonce never claims nonpayment", async () => {
  expect(await getBaseTransactionStatus(options())).toBe("unknown");
  f.rpc.getTransactionCount.mockResolvedValue(8);
  expect(await getBaseTransactionStatus(options())).toBe("nonce-consumed");
  expect(f.rpc.getTransactionCount).toHaveBeenCalledWith({
    address: signer.address,
    blockTag: "finalized",
  });
  expect(f.rpc.sendRawTransaction).not.toHaveBeenCalled();
});
test("known pending transaction is bound to the saved record", async () => {
  f.rpc.getTransaction.mockResolvedValue({
    hash: record.hash,
    from: record.from,
    chainId: 84532,
    nonce: 7,
  });
  expect(await getBaseTransactionStatus(options())).toBe("pending");
  f.rpc.getTransaction.mockResolvedValue({
    hash: record.hash,
    from: record.from,
    chainId: 84532,
    nonce: 8,
  });
  await expect(getBaseTransactionStatus(options())).rejects.toMatchObject({
    code: "verification_failed",
  });
});
test.each(["success", "reverted"])(
  "only a canonical finalized %s receipt yields a final outcome",
  async (status) => {
    f.rpc.getTransactionReceipt.mockResolvedValue({
      status,
      transactionHash: record.hash,
      blockNumber: 100n,
      blockHash,
    });
    expect(await getBaseTransactionStatus(options())).toBe(
      status === "success" ? "confirmed" : "failed",
    );
    f.rpc.getBlock.mockImplementation(async ({ blockTag }) =>
      blockTag === "finalized" ? { number: 99n } : { hash: blockHash },
    );
    expect(await getBaseTransactionStatus(options())).toBe("included");
    f.rpc.getBlock.mockImplementation(async ({ blockTag }) =>
      blockTag === "finalized" ? { number: 100n } : { hash: `0x${"bb".repeat(32)}` },
    );
    await expect(getBaseTransactionStatus(options())).rejects.toMatchObject({
      code: "verification_failed",
    });
  },
);
test("one explicit retry uses identical signed bytes after a lost ACK", async () => {
  f.rpc.sendRawTransaction.mockRejectedValueOnce(
    new Error("https://rpc.invalid/?key=private request-body"),
  );
  await expect(rebroadcastBaseTransaction(options())).rejects.toMatchObject({
    code: "submission_unknown",
  });
  expect(await rebroadcastBaseTransaction(options())).toBe(record.hash);
  expect(f.rpc.sendRawTransaction.mock.calls.map(([arg]) => arg.serializedTransaction)).toEqual([
    raw,
    raw,
  ]);
  expect(f.http).toHaveBeenCalledWith(network.rpcUrl, expect.objectContaining({ retryCount: 0 }));
});
test("RPC errors have fixed public copy and non-enumerable diagnostic causes", async () => {
  const secret = "https://rpc.invalid/?key=private request-body account-id";
  f.rpc.getTransactionReceipt.mockRejectedValue(new Error(secret));
  const error = await getBaseTransactionStatus(options()).catch((e) => e);
  expect(error).toBeInstanceOf(BaseError);
  expect(error.code).toBe("rpc_unavailable");
  expect(baseErrorMessage(error)).not.toContain(secret);
  expect(JSON.stringify(error)).not.toContain(secret);
  expect(error.message).not.toContain("account-id");
});
test("aborted guard cannot contact RPC or retry", async () => {
  const c = new AbortController();
  c.abort();
  await expect(
    rebroadcastBaseTransaction({ ...options(), guard: { ...guard, signal: c.signal } }),
  ).rejects.toMatchObject({ code: "cancelled" });
  expect(f.client).not.toHaveBeenCalled();
});
test("no lower-level transfer can silently choose an RPC", () => {
  expect(() => createBaseTransfers(BASE_SEPOLIA as any)).toThrow();
  expect(f.client).not.toHaveBeenCalled();
});

test("USDC payment proof requires the exact canonical Transfer event, not just execution success", async () => {
  const recipient = "0x2222222222222222222222222222222222222222" as const;
  const abi = parseAbi([
    "function transfer(address to, uint256 amount) returns (bool)",
    "event Transfer(address indexed from, address indexed to, uint256 value)",
  ]);
  const serializedTransaction = await signer.signTransaction({
    chainId: 84532,
    type: "eip1559",
    to: network.usdc,
    value: 0n,
    data: encodeFunctionData({ abi, functionName: "transfer", args: [recipient, 123n] }),
    nonce: 7,
    gas: 60000n,
    maxFeePerGas: 10n,
    maxPriorityFeePerGas: 1n,
  });
  const transaction = await baseTransactionRecord(serializedTransaction);
  const receipt = {
    status: "success",
    transactionHash: transaction.hash,
    blockNumber: 100n,
    blockHash,
    logs: [] as any[],
  };
  f.rpc.getTransactionReceipt.mockResolvedValue(receipt);
  expect(await getBaseTransactionStatus({ ...options(), transaction })).toBe("confirmed");
  await expect(getBasePaymentStatus({ ...options(), transaction })).rejects.toMatchObject({
    code: "verification_failed",
  });
  const log = {
    address: network.usdc,
    transactionHash: transaction.hash,
    blockHash,
    removed: false,
    topics: encodeEventTopics({
      abi,
      eventName: "Transfer",
      args: { from: signer.address, to: recipient },
    }),
    data: encodeAbiParameters([{ type: "uint256" }], [123n]),
  };
  receipt.logs = [log];
  expect(await getBasePaymentStatus({ ...options(), transaction })).toBe("confirmed");
  for (const bad of [
    { removed: true },
    { transactionHash: record.hash },
    { address: recipient },
    { blockHash: `0x${"bb".repeat(32)}` },
    {
      topics: encodeEventTopics({
        abi,
        eventName: "Transfer",
        args: { from: recipient, to: recipient },
      }),
    },
    {
      topics: encodeEventTopics({
        abi,
        eventName: "Transfer",
        args: { from: signer.address, to: signer.address },
      }),
    },
    { data: encodeAbiParameters([{ type: "uint256" }], [124n]) },
  ]) {
    receipt.logs = [{ ...log, ...bad }];
    await expect(getBasePaymentStatus({ ...options(), transaction })).rejects.toMatchObject({
      code: "verification_failed",
    });
  }
});

test("unsupported USDC calldata is a verification failure, not an RPC failure", async () => {
  const serializedTransaction = await signer.signTransaction({
    chainId: 84532,
    type: "eip1559",
    to: network.usdc,
    value: 0n,
    data: encodeFunctionData({
      abi: parseAbi(["function approve(address spender, uint256 amount) returns (bool)"]),
      functionName: "approve",
      args: [signer.address, 123n],
    }),
    nonce: 7,
    gas: 60000n,
    maxFeePerGas: 10n,
    maxPriorityFeePerGas: 1n,
  });
  const transaction = await baseTransactionRecord(serializedTransaction);
  f.rpc.getTransactionReceipt.mockResolvedValue({
    status: "success",
    transactionHash: transaction.hash,
    blockNumber: 100n,
    blockHash,
    logs: [],
  });
  await expect(getBasePaymentStatus({ ...options(), transaction })).rejects.toMatchObject({
    code: "verification_failed",
  });
  expect(f.rpc.sendRawTransaction).not.toHaveBeenCalled();
});
