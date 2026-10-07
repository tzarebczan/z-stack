import { afterEach, beforeEach, expect, test, vi } from "vitest";
import type { HDAccount } from "viem";

const fixture = vi.hoisted(() => ({
  rpc: {
    getChainId: vi.fn(),
    getBalance: vi.fn(),
    readContract: vi.fn(),
    estimateGas: vi.fn(),
    estimateFeesPerGas: vi.fn(),
    getTransactionCount: vi.fn(),
    estimateL1Fee: vi.fn(),
    sendRawTransaction: vi.fn(),
  },
  create: vi.fn(),
  http: vi.fn(),
  extend: vi.fn(),
  sign: vi.fn(),
}));
vi.mock("viem", async (importOriginal) => {
  const real = await importOriginal<typeof import("viem")>();
  return { ...real, createPublicClient: fixture.create, http: fixture.http };
});
import { BaseTransferSubmissionUnknownError } from "../src/usdc";
import { decodeFunctionData, encodeFunctionData, keccak256 } from "viem";
import { prepareBaseSwap, submitBaseSwap } from "./test-bindings";
import { mnemonicToAccount } from "viem/accounts";
const signer = mnemonicToAccount("test test test test test test test test test test test junk");
const FROM = signer.address;
const TO = "0x2222222222222222222222222222222222222222";
const USDC = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
const TRANSFER = [
  {
    name: "transfer",
    type: "function",
    stateMutability: "nonpayable",
    inputs: [
      { name: "to", type: "address" },
      { name: "amount", type: "uint256" },
    ],
    outputs: [{ name: "", type: "bool" }],
  },
] as const;
const SIGNED = await signer.signTransaction({
  chainId: 8453,
  type: "eip1559",
  to: USDC,
  value: 0n,
  data: encodeFunctionData({ abi: TRANSFER, functionName: "transfer", args: [TO, 1_000_000n] }),
  nonce: 7,
  gas: 25_202n,
  maxFeePerGas: 10n,
  maxPriorityFeePerGas: 2n,
});
const persist = vi.fn(async () => {});
const account = () => ({ address: FROM, signTransaction: fixture.sign }) as unknown as HDAccount;
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}
beforeEach(() => {
  vi.clearAllMocks();
  fixture.create.mockReturnValue({ extend: fixture.extend });
  fixture.extend.mockReturnValue(fixture.rpc);
  fixture.http.mockReturnValue({ type: "http" });
  fixture.rpc.getChainId.mockResolvedValue(8453);
  fixture.rpc.getBalance.mockResolvedValue(1_000_000n);
  fixture.rpc.readContract.mockImplementation(async ({ functionName }) =>
    functionName === "balanceOf" ? 10_000_000n : 47_980n,
  );
  fixture.rpc.estimateGas.mockResolvedValue(21_001n);
  fixture.rpc.estimateFeesPerGas.mockResolvedValue({ maxFeePerGas: 10n, maxPriorityFeePerGas: 2n });
  fixture.rpc.getTransactionCount.mockResolvedValue(7);
  fixture.rpc.estimateL1Fee.mockResolvedValue(200_000n);
  fixture.rpc.sendRawTransaction.mockImplementation(async ({ serializedTransaction }) =>
    keccak256(serializedTransaction),
  );
  fixture.sign.mockImplementation((tx) => signer.signTransaction(tx));
});
afterEach(() => vi.restoreAllMocks());

test("operator oracle read failure never produces a review or signing authority", async () => {
  fixture.rpc.readContract.mockImplementation(async ({ functionName }) => {
    if (functionName === "getOperatorFee") throw new Error("HTTP 429");
    return 10_000_000n;
  });
  await expect(prepareBaseSwap(FROM, TO, 1_000_000n)).rejects.toThrow(
    "Base network is unavailable",
  );
  expect(fixture.sign).not.toHaveBeenCalled();
  expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
});

test("confirmed zero operator fee retains the separate L1 and L2 costs", async () => {
  fixture.rpc.readContract.mockImplementation(async ({ functionName }) =>
    functionName === "balanceOf" ? 10_000_000n : 0n,
  );
  const result = await prepareBaseSwap(FROM, TO, 1_000_000n);
  expect(result).toMatchObject({ variableFee: 200_000n, l2Max: 252_020n, fee: 452_020n });
});

test("constructs only native Base USDC transfer calldata with zero ETH and total OP fee", async () => {
  const prepared = await prepareBaseSwap(FROM, TO, 1_000_001n);
  expect(prepared.transaction).toMatchObject({
    chainId: 8453,
    to: USDC,
    value: 0n,
    nonce: 7,
    gas: 25_202n,
    maxFeePerGas: 10n,
    maxPriorityFeePerGas: 2n,
    type: "eip1559",
  });
  expect(decodeFunctionData({ abi: TRANSFER, data: prepared.transaction.data })).toEqual({
    functionName: "transfer",
    args: [TO, 1_000_001n],
  });
  expect(fixture.rpc.readContract).toHaveBeenCalledWith(
    expect.objectContaining({ address: USDC, functionName: "balanceOf", args: [FROM] }),
  );
  expect(fixture.rpc.getTransactionCount).toHaveBeenCalledWith({
    address: FROM,
    blockTag: "pending",
  });
  expect(fixture.rpc.estimateL1Fee).toHaveBeenCalledWith({
    account: FROM,
    ...prepared.transaction,
  });
  expect(prepared.fee).toBe(500_000n);
  expect(prepared.l2Max).toBe(252_020n);
  expect(prepared.variableFee).toBe(247_980n);
  expect(fixture.rpc.readContract).toHaveBeenCalledWith(
    expect.objectContaining({
      address: "0x420000000000000000000000000000000000000F",
      functionName: "getOperatorFee",
      args: [prepared.transaction.gas],
    }),
  );
  expect(Object.isFrozen(prepared.transaction)).toBe(true);
  expect(Object.isFrozen(prepared)).toBe(true);
  expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
  expect(fixture.sign).not.toHaveBeenCalled();
});
test.each(["chain", "usdc", "total-eth", "max-gas-reserve", "zero-fee"])(
  "blocks preparation when %s is invalid or insufficient",
  async (kind) => {
    if (kind === "chain") fixture.rpc.getChainId.mockResolvedValueOnce(1);
    if (kind === "usdc") fixture.rpc.readContract.mockResolvedValueOnce(1n);
    if (kind === "total-eth") fixture.rpc.getBalance.mockResolvedValueOnce(300_000n); // L2 max gas cost is lower, but total OP fee is not affordable.
    // The total fee estimate is affordable, but signed max gas plus OP reserve is not.
    if (kind === "max-gas-reserve") fixture.rpc.getBalance.mockResolvedValueOnce(400_000n);
    if (kind === "zero-fee") {
      fixture.rpc.estimateL1Fee.mockResolvedValueOnce(0n);
      fixture.rpc.readContract.mockImplementation(async ({ functionName }) =>
        functionName === "balanceOf" ? 10_000_000n : 0n,
      );
      fixture.rpc.estimateFeesPerGas.mockResolvedValueOnce({
        maxFeePerGas: 0n,
        maxPriorityFeePerGas: 0n,
      });
    }
    await expect(prepareBaseSwap(FROM, TO, 1_000_000n)).rejects.toThrow();
    expect(fixture.sign).not.toHaveBeenCalled();
    expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
    if (kind === "chain") expect(fixture.rpc.getBalance).not.toHaveBeenCalled();
  },
);
test.each([0n, -1n])("rejects invalid amount %s before RPC", async (amount) => {
  await expect(prepareBaseSwap(FROM, TO, amount)).rejects.toMatchObject({
    code: "invalid_payment",
  });
  expect(fixture.create).not.toHaveBeenCalled();
});
test("does not accept a substituted contract transaction or wallet", async () => {
  const prepared = await prepareBaseSwap(FROM, TO, 1_000_000n);
  await expect(submitBaseSwap(account(), { ...prepared }, () => true, persist)).rejects.toThrow(
    "changed",
  );
  await expect(
    submitBaseSwap({ ...account(), address: TO } as HDAccount, prepared, () => true, persist),
  ).rejects.toThrow("changed");
  expect(fixture.sign).not.toHaveBeenCalled();
  expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
});
test("signs/broadcasts exact prepared bytes once and disables transport retries", async () => {
  const prepared = await prepareBaseSwap(FROM, TO, 1_000_000n);
  expect(await submitBaseSwap(account(), prepared, () => true, persist)).toBe(keccak256(SIGNED));
  expect(fixture.sign).toHaveBeenCalledWith(prepared.transaction);
  expect(fixture.rpc.sendRawTransaction).toHaveBeenCalledWith({
    serializedTransaction: SIGNED,
  });
  expect(fixture.http).toHaveBeenLastCalledWith(
    "https://mainnet.base.org",
    expect.objectContaining({ retryCount: 0, timeout: 15_000 }),
  );
  await expect(submitBaseSwap(account(), prepared, () => true, persist)).rejects.toThrow("changed");
  expect(fixture.sign).toHaveBeenCalledTimes(1);
  expect(fixture.rpc.sendRawTransaction).toHaveBeenCalledTimes(1);
});
test("cancellation before or during signing never broadcasts", async () => {
  const prepared = await prepareBaseSwap(FROM, TO, 1_000_000n);
  await expect(submitBaseSwap(account(), prepared, () => false, persist)).rejects.toMatchObject({
    code: "wallet_changed",
  });
  expect(fixture.sign).not.toHaveBeenCalled();
  let current = true;
  const pending = deferred<`0x${string}`>();
  fixture.sign.mockReturnValueOnce(pending.promise);
  const operation = submitBaseSwap(account(), prepared, () => current, persist);
  current = false;
  pending.resolve(SIGNED);
  await expect(operation).rejects.toMatchObject({ code: "wallet_changed" });
  expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
});
test("a lost send acknowledgement cannot sign or broadcast the prepared transaction again", async () => {
  const prepared = await prepareBaseSwap(FROM, TO, 1_000_000n);
  fixture.rpc.sendRawTransaction.mockRejectedValueOnce(new Error("ack lost"));
  await expect(submitBaseSwap(account(), prepared, () => true, persist)).rejects.toBeInstanceOf(
    BaseTransferSubmissionUnknownError,
  );
  await expect(submitBaseSwap(account(), prepared, () => true, persist)).rejects.toThrow("changed");
  expect(fixture.sign).toHaveBeenCalledTimes(1);
  expect(fixture.rpc.sendRawTransaction).toHaveBeenCalledTimes(1);
});
test("a late acknowledgement remains available for recording against the original receipt", async () => {
  const prepared = await prepareBaseSwap(FROM, TO, 1_000_000n);
  const pending = deferred<`0x${string}`>();
  fixture.rpc.sendRawTransaction.mockReturnValueOnce(pending.promise);
  let current = true;
  const operation = submitBaseSwap(account(), prepared, () => current, persist);
  await vi.waitFor(() => expect(fixture.rpc.sendRawTransaction).toHaveBeenCalled());
  current = false;
  pending.resolve(keccak256(SIGNED));
  await expect(operation).resolves.toBe(keccak256(SIGNED));
});
test.each(["signal", "owner"])(
  "already cancelled %s does not create a Base RPC client",
  async (kind) => {
    const controller = new AbortController();
    if (kind === "signal") controller.abort();
    await expect(
      prepareBaseSwap(FROM, TO, 1_000_000n, {
        signal: controller.signal,
        stillCurrent: () => kind !== "owner",
      }),
    ).rejects.toThrow("wallet changed");
    expect(fixture.create).not.toHaveBeenCalled();
  },
);
test.each(["chain", "balances", "total-fee"])(
  "cancellation at actual %s await prevents later RPCs or prepared authority",
  async (boundary) => {
    const controller = new AbortController();
    let current = true;
    const pending = deferred<bigint | number>();
    if (boundary === "chain") fixture.rpc.getChainId.mockReturnValueOnce(pending.promise);
    if (boundary === "balances") fixture.rpc.getBalance.mockReturnValueOnce(pending.promise);
    if (boundary === "total-fee") fixture.rpc.estimateL1Fee.mockReturnValueOnce(pending.promise);
    const operation = prepareBaseSwap(FROM, TO, 1_000_000n, {
      signal: controller.signal,
      stillCurrent: () => current,
    });
    await vi.waitFor(() =>
      expect(
        boundary === "chain"
          ? fixture.rpc.getChainId
          : boundary === "balances"
            ? fixture.rpc.getBalance
            : fixture.rpc.estimateL1Fee,
      ).toHaveBeenCalled(),
    );
    current = false;
    controller.abort();
    pending.resolve(boundary === "chain" ? 8453 : 1_000_000n);
    await expect(operation).rejects.toThrow("wallet changed");
    if (boundary === "chain") {
      expect(fixture.rpc.getBalance).not.toHaveBeenCalled();
      expect(fixture.rpc.estimateFeesPerGas).not.toHaveBeenCalled();
    }
    if (boundary !== "total-fee") expect(fixture.rpc.estimateL1Fee).not.toHaveBeenCalled();
    expect(fixture.http).toHaveBeenCalledWith(
      "https://mainnet.base.org",
      expect.objectContaining({ fetchOptions: { signal: controller.signal }, retryCount: 0 }),
    );
    expect(fixture.sign).not.toHaveBeenCalled();
    expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
  },
);

test.each(["not-a-hash", `0x${"ff".repeat(32)}`])(
  "RPC acknowledgement %s cannot replace the locally signed hash",
  async (acknowledged) => {
    const prepared = await prepareBaseSwap(FROM, TO, 1_000_000n);
    fixture.rpc.sendRawTransaction.mockResolvedValueOnce(acknowledged);
    await expect(submitBaseSwap(account(), prepared, () => true, persist)).rejects.toMatchObject({
      name: "BaseTransferSubmissionUnknownError",
      hash: keccak256(SIGNED),
      chainId: 8453,
    });
    await expect(submitBaseSwap(account(), prepared, () => true, persist)).rejects.toThrow(
      "changed",
    );
    expect(fixture.sign).toHaveBeenCalledOnce();
    expect(fixture.rpc.sendRawTransaction).toHaveBeenCalledOnce();
  },
);

test("lower-level pre-broadcast persistence sees the real signed record and must finish before RPC", async () => {
  const prepared = await prepareBaseSwap(FROM, TO, 1_000_000n);
  const gate = deferred<void>();
  const save = vi.fn(() => gate.promise);
  const send = submitBaseSwap(account(), prepared, () => true, save);
  await vi.waitFor(() => expect(save).toHaveBeenCalled());
  expect(save).toHaveBeenCalledWith(
    expect.objectContaining({
      hash: keccak256(SIGNED),
      serializedTransaction: SIGNED,
      nonce: 7,
      from: FROM,
    }),
  );
  expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
  gate.resolve();
  expect(await send).toBe(keccak256(SIGNED));
});
test("lower-level failed hash persistence never broadcasts or exposes raw diagnostic text", async () => {
  const prepared = await prepareBaseSwap(FROM, TO, 1_000_000n);
  const error = await submitBaseSwap(
    account(),
    prepared,
    () => true,
    async () => {
      throw new Error("secret RPC credential");
    },
  ).catch((e) => e);
  expect(error.code).toBe("submission_not_sent");
  expect(JSON.stringify(error)).not.toContain("secret");
  expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
});
test("lower-level signer cannot change serialized amount before the journal callback", async () => {
  const prepared = await prepareBaseSwap(FROM, TO, 1_000_000n);
  fixture.sign.mockImplementationOnce((tx) => signer.signTransaction({ ...tx, value: 1n }));
  await expect(submitBaseSwap(account(), prepared, () => true, persist)).rejects.toMatchObject({
    code: "transaction_mismatch",
  });
  expect(persist).not.toHaveBeenCalled();
  expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
});
