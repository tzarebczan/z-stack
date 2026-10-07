import { afterEach, beforeEach, expect, test, vi } from "vitest";

const fixture = vi.hoisted(() => ({
  rpc: {
    getChainId: vi.fn(),
    getBalance: vi.fn(),
    estimateGas: vi.fn(),
    estimateFeesPerGas: vi.fn(),
    getTransactionCount: vi.fn(),
    estimateL1Fee: vi.fn(),
    readContract: vi.fn(),
  },
  create: vi.fn(),
  http: vi.fn(),
  extend: vi.fn(),
}));
vi.mock("viem", async (original) => ({
  ...(await original<typeof import("viem")>()),
  createPublicClient: fixture.create,
  http: fixture.http,
}));
import { prepareBaseEth } from "./test-bindings";

const FROM = "0x1111111111111111111111111111111111111111";
const TO = "0x2222222222222222222222222222222222222222";
beforeEach(() => {
  vi.resetAllMocks();
  fixture.create.mockReturnValue({ extend: fixture.extend });
  fixture.extend.mockReturnValue(fixture.rpc);
  fixture.rpc.getChainId.mockResolvedValue(8453);
  fixture.rpc.getBalance.mockResolvedValue(2_000_000n);
  fixture.rpc.estimateGas.mockResolvedValue(21_000n);
  fixture.rpc.estimateFeesPerGas.mockResolvedValue({ maxFeePerGas: 10n, maxPriorityFeePerGas: 2n });
  fixture.rpc.getTransactionCount.mockResolvedValue(7);
  fixture.rpc.estimateL1Fee.mockResolvedValue(200_000n);
  fixture.rpc.readContract.mockResolvedValue(48_000n);
});
afterEach(() => vi.restoreAllMocks());

test("operator oracle read failure never produces an ETH review", async () => {
  fixture.rpc.readContract.mockRejectedValueOnce(new Error("HTTP 429"));
  await expect(prepareBaseEth(FROM, TO, 1n)).rejects.toThrow("Base network is unavailable");
});

test("confirmed zero operator fee retains the ETH L1 and L2 costs", async () => {
  fixture.rpc.readContract.mockResolvedValueOnce(0n);
  expect(await prepareBaseEth(FROM, TO, 1n)).toMatchObject({
    variableFee: 200_000n,
    l2Max: 252_000n,
    fee: 452_000n,
  });
});

test("native ETH review constructs exact local bytes and covers payment plus full OP fee", async () => {
  const result = await prepareBaseEth(FROM, TO, 1_000_000n);
  expect(result.transaction).toEqual({
    chainId: 8453,
    type: "eip1559",
    to: TO,
    value: 1_000_000n,
    nonce: 7,
    gas: 25_200n,
    maxFeePerGas: 10n,
    maxPriorityFeePerGas: 2n,
  });
  expect(result).toMatchObject({ fee: 500_000n, l2Max: 252_000n, variableFee: 248_000n });
  expect(fixture.rpc.estimateGas).toHaveBeenCalledExactlyOnceWith({
    account: FROM,
    to: TO,
    value: 1_000_000n,
  });
  expect(fixture.rpc.getTransactionCount).toHaveBeenCalledWith({
    address: FROM,
    blockTag: "pending",
  });
  expect(fixture.rpc.estimateL1Fee).toHaveBeenCalledWith({ account: FROM, ...result.transaction });
  expect(Object.isFrozen(result.transaction)).toBe(true);
  expect(Object.isFrozen(result)).toBe(true);
  expect(fixture.http).toHaveBeenCalledWith(
    "https://mainnet.base.org",
    expect.objectContaining({ retryCount: 0, timeout: 15_000 }),
  );
});

test.each([1_000_001n, 1_499_999n])(
  "ETH balance %s cannot cover value and maximum estimated fees",
  async (balance) => {
    fixture.rpc.getBalance.mockResolvedValue(balance);
    await expect(prepareBaseEth(FROM, TO, 1_000_000n)).rejects.toThrow("payment and network fee");
  },
);
test("exact value plus total fee balance is sufficient", async () => {
  fixture.rpc.getBalance.mockResolvedValue(1_500_000n);
  expect((await prepareBaseEth(FROM, TO, 1_000_000n)).fee).toBe(500_000n);
});
test("wrong chain fails before estimates", async () => {
  fixture.rpc.getChainId.mockResolvedValue(1);
  await expect(prepareBaseEth(FROM, TO, 1n)).rejects.toThrow("network could not be verified");
  expect(fixture.rpc.estimateGas).not.toHaveBeenCalled();
});

test.each([
  ["getBalance", -1n],
  ["getBalance", "2000000"],
  ["estimateGas", 0n],
  ["estimateGas", -1n],
  ["estimateGas", 1n << 64n],
  ["estimateGas", (1n << 64n) - 1n],
  ["getTransactionCount", -1],
  ["getTransactionCount", 1.5],
  ["getTransactionCount", Number.MAX_SAFE_INTEGER + 1],
  ["estimateL1Fee", -1n],
  ["estimateL1Fee", "1"],
  ["estimateL1Fee", 1n << 256n],
  ["readContract", -1n],
  ["readContract", "1"],
])("invalid RPC %s=%s fails closed", async (method, value) => {
  fixture.rpc[method as keyof typeof fixture.rpc].mockResolvedValue(value);
  await expect(prepareBaseEth(FROM, TO, 1n)).rejects.toThrow("invalid fee estimates");
});
test.each([
  { maxFeePerGas: 0n, maxPriorityFeePerGas: 0n },
  { maxFeePerGas: -1n, maxPriorityFeePerGas: 0n },
  { maxFeePerGas: "10", maxPriorityFeePerGas: 2n },
  { maxFeePerGas: 10n, maxPriorityFeePerGas: -1n },
  { maxFeePerGas: 10n, maxPriorityFeePerGas: 11n },
  { maxFeePerGas: 1n << 256n, maxPriorityFeePerGas: 2n },
])("invalid fee parameters %o fail closed", async (fees) => {
  fixture.rpc.estimateFeesPerGas.mockResolvedValue(fees);
  await expect(prepareBaseEth(FROM, TO, 1n)).rejects.toThrow("invalid fee estimates");
});
test.each([0n, -1n, 1n << 256n])("invalid amount %s makes no RPC request", async (amount) => {
  await expect(prepareBaseEth(FROM, TO, amount)).rejects.toThrow("valid Base");
  expect(fixture.create).not.toHaveBeenCalled();
});
test("identity change during fee lookup never publishes a stale review", async () => {
  let current = true;
  fixture.rpc.readContract.mockImplementation(async () => {
    current = false;
    return 1n;
  });
  await expect(prepareBaseEth(FROM, TO, 1n, { stillCurrent: () => current })).rejects.toThrow(
    "wallet changed",
  );
});
test("already aborted preparation does not create a client", async () => {
  const controller = new AbortController();
  controller.abort();
  await expect(prepareBaseEth(FROM, TO, 1n, { signal: controller.signal })).rejects.toThrow(
    "wallet changed",
  );
  expect(fixture.create).not.toHaveBeenCalled();
});
