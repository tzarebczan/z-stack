import { beforeEach, expect, test, vi } from "vitest";
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
}));
vi.mock("viem", async (original) => ({
  ...(await original<typeof import("viem")>()),
  createPublicClient: fixture.create,
  http: fixture.http,
}));
import { keccak256, type Address } from "viem";
import {
  BASE_SEPOLIA,
  createBaseWallet,
  deriveBaseAccount,
  deriveBaseAddress,
  BaseSubmissionUnknownError,
  formatBaseBalance,
  type BaseSendOptions,
} from "../src/index";
import type { HDAccount } from "viem/accounts";
const phrase = "test test test test test test test test test test test junk"; // public test vector
const owner = deriveBaseAccount(phrase);
const recipient = "0x2222222222222222222222222222222222222222" as Address;
const guard = { assertCurrent: vi.fn() };
const wallet = () =>
  createBaseWallet({
    address: owner.address,
    network: BASE_SEPOLIA,
    rpcUrl: "http://localhost:8545",
  });
const send = (): BaseSendOptions => ({
  guard,
  withSpendLock: async (run) => run(),
  withSigner: async (run) => run(owner),
  assertNoPending: vi.fn(),
  reserveSubmission: vi.fn(async () => {}),
  releaseSubmission: vi.fn(async () => {}),
});
beforeEach(() => {
  vi.clearAllMocks();
  fixture.create.mockReturnValue({ ...fixture.rpc, extend: () => fixture.rpc });
  fixture.rpc.getChainId.mockResolvedValue(84532);
  fixture.rpc.getBalance.mockResolvedValue(10n ** 20n);
  fixture.rpc.readContract.mockImplementation(async ({ functionName }) =>
    functionName === "balanceOf" ? 10_000_000n : 1n,
  );
  fixture.rpc.estimateGas.mockResolvedValue(21_000n);
  fixture.rpc.estimateFeesPerGas.mockResolvedValue({ maxFeePerGas: 10n, maxPriorityFeePerGas: 1n });
  fixture.rpc.getTransactionCount.mockResolvedValue(0);
  fixture.rpc.estimateL1Fee.mockResolvedValue(100n);
  fixture.rpc.sendRawTransaction.mockImplementation(async ({ serializedTransaction }) =>
    keccak256(serializedTransaction),
  );
});
const review = async (w = wallet(), asset: "eth" | "usdc" = "usdc") =>
  w.reviewPayment({ asset, recipient, amount: 1_000_000n, deadline: Date.now() + 60_000 }, guard);
test("local derivation and wallet construction do not contact any service", () => {
  expect(deriveBaseAddress(phrase)).toBe(owner.address);
  wallet();
  expect(fixture.create).not.toHaveBeenCalled();
  expect(
    formatBaseBalance({ eth: 123456789012345678901234567890n, usdc: 9007199254740991000001n }),
  ).toEqual({
    eth: "123456789012.34567890123456789",
    usdc: "9007199254740991.000001",
  });
});
test("reads the explicit testnet RPC and USDC with a verified chain", async () => {
  expect(await wallet().getBalance(guard)).toEqual({ eth: 10n ** 20n, usdc: 10_000_000n });
  expect(fixture.http).toHaveBeenCalledWith(
    "http://localhost:8545",
    expect.objectContaining({ retryCount: 0 }),
  );
  expect(fixture.rpc.readContract).toHaveBeenCalledWith(
    expect.objectContaining({ address: BASE_SEPOLIA.usdc }),
  );
});
test.each(["eth", "usdc"] as const)(
  "review/unlock/reserve/one broadcast of %s uses real signatures",
  async (asset) => {
    const w = wallet(),
      r = await review(w, asset),
      options = send();
    const hash = await w.sendPayment(r, options);
    expect(options.reserveSubmission).toHaveBeenCalledWith(
      expect.objectContaining({ hash, chainId: 84532, review: r }),
    );
    expect(fixture.rpc.sendRawTransaction).toHaveBeenCalledTimes(1);
    await expect(w.sendPayment(r, options)).rejects.toThrow("original review");
  },
);
test("fee increase requires a new review before invoking unlock", async () => {
  const w = wallet(),
    r = await review(w),
    options = send();
  options.withSigner = vi.fn(options.withSigner);
  fixture.rpc.estimateFeesPerGas.mockResolvedValue({ maxFeePerGas: 11n, maxPriorityFeePerGas: 1n });
  await expect(w.sendPayment(r, options)).rejects.toThrow("fee increased");
  expect(options.withSigner).not.toHaveBeenCalled();
});
test("copies and reviews from another wallet cannot authorize a payment", async () => {
  const w = wallet(),
    r = await review(w);
  await expect(w.sendPayment({ ...r }, send())).rejects.toThrow("original review");
  await expect(wallet().sendPayment(r, send())).rejects.toThrow("original review");
});
test("a failed durable save cannot broadcast, and the consumed review cannot sign again", async () => {
  const w = wallet(),
    r = await review(w),
    options = send();
  options.reserveSubmission = vi.fn(async () => {
    throw new Error("disk full");
  });
  await expect(w.sendPayment(r, options)).rejects.toMatchObject({ code: "submission_not_sent" });
  expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
  await expect(w.sendPayment(r, send())).rejects.toThrow("original review");
});
test.each(["lost", "wrong-hash"])(
  "unknown %s preserves the saved expected hash and cannot retry",
  async (kind) => {
    const w = wallet(),
      r = await review(w),
      options = send();
    fixture.rpc.sendRawTransaction.mockImplementation(async () => {
      if (kind === "lost") throw new Error("lost");
      return "0x" + "a".repeat(64);
    });
    const error = await w.sendPayment(r, options).catch((e) => e);
    expect(error).toBeInstanceOf(BaseSubmissionUnknownError);
    expect(options.reserveSubmission).toHaveBeenCalledWith(error.submission);
    expect(JSON.stringify(error)).not.toContain(error.submission.serializedTransaction);
    expect(JSON.stringify(error)).toContain(error.submission.hash);
    expect(options.releaseSubmission).not.toHaveBeenCalled();
    await expect(w.sendPayment(r, send())).rejects.toThrow("original review");
    expect(fixture.rpc.sendRawTransaction).toHaveBeenCalledTimes(1);
  },
);
test("pending sponsored or native reservation blocks signing", async () => {
  const w = wallet(),
    r = await review(w),
    options = send();
  options.assertNoPending = async () => {
    throw new Error("pending");
  };
  options.withSigner = vi.fn(options.withSigner);
  await expect(w.sendPayment(r, options)).rejects.toMatchObject({ code: "payment_blocked" });
  expect(options.withSigner).not.toHaveBeenCalled();
});
test("signer address substitution fails before reservation or broadcast", async () => {
  const w = wallet(),
    r = await review(w),
    options = send();
  options.withSigner = async (run) => run({ ...owner, address: recipient });
  await expect(w.sendPayment(r, options)).rejects.toMatchObject({ code: "wallet_changed" });
  expect(options.reserveSubmission).not.toHaveBeenCalled();
});
test("signer cannot replace the recipient in serialized bytes", async () => {
  const w = wallet(),
    r = await review(w, "eth"),
    options = send();
  options.withSigner = async (run) =>
    run({
      ...owner,
      signTransaction: async (tx) => owner.signTransaction({ ...tx, to: owner.address }),
    } as HDAccount);
  await expect(w.sendPayment(r, options)).rejects.toThrow("signer changed");
  expect(options.reserveSubmission).not.toHaveBeenCalled();
});
test("cancellation during unlock cannot sign or broadcast", async () => {
  const w = wallet(),
    r = await review(w),
    options = send(),
    controller = new AbortController();
  options.guard = { ...guard, signal: controller.signal };
  options.withSigner = async (run) => {
    controller.abort();
    return run(owner);
  };
  await expect(w.sendPayment(r, options)).rejects.toThrow("cancelled");
  expect(options.reserveSubmission).not.toHaveBeenCalled();
});
test("cancellation after durable save releases only the matching reservation without broadcasting", async () => {
  const w = wallet(),
    r = await review(w),
    options = send(),
    controller = new AbortController();
  options.guard = { ...guard, signal: controller.signal };
  options.reserveSubmission = vi.fn(async () => {
    controller.abort();
  });
  await expect(w.sendPayment(r, options)).rejects.toMatchObject({ code: "submission_not_sent" });
  expect(options.reserveSubmission).toHaveBeenCalledTimes(1);
  expect(options.releaseSubmission).toHaveBeenCalledWith(
    expect.objectContaining({ nonce: 0, serializedTransaction: expect.stringMatching(/^0x02/) }),
  );
  expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
});
test("concurrent sends of one review can only sign/broadcast once", async () => {
  const w = wallet(),
    r = await review(w),
    options = send();
  let queue = Promise.resolve();
  options.withSpendLock = (run) => {
    const next = queue.then(run);
    queue = next.then(
      () => {},
      () => {},
    );
    return next;
  };
  const results = await Promise.allSettled([w.sendPayment(r, options), w.sendPayment(r, options)]);
  expect(results.filter((r) => r.status === "fulfilled")).toHaveLength(1);
  expect(fixture.rpc.sendRawTransaction).toHaveBeenCalledTimes(1);
});
test("a wrong chain or malformed balance fails before displaying data", async () => {
  fixture.rpc.getChainId.mockResolvedValue(8453);
  await expect(wallet().getBalance(guard)).rejects.toMatchObject({ code: "chain_mismatch" });
  fixture.rpc.getChainId.mockResolvedValue(84532);
  fixture.rpc.getBalance.mockResolvedValue("100");
  await expect(wallet().getBalance(guard)).rejects.toMatchObject({ code: "invalid_response" });
});
test.each(["http://example.com", "https://user:secret@example.com", "ftp://example.com"])(
  "rejects insecure RPC %s without a request",
  (url) => {
    expect(() =>
      createBaseWallet({ address: owner.address, network: BASE_SEPOLIA, rpcUrl: url }),
    ).toThrow();
    expect(fixture.create).not.toHaveBeenCalled();
  },
);

test("caller mutations cannot change a constructed wallet's signing chain", async () => {
  const input = { ...BASE_SEPOLIA, chain: { ...BASE_SEPOLIA.chain } };
  const w = createBaseWallet({
    address: owner.address,
    network: input,
    rpcUrl: "http://localhost:8545",
  });
  const r = await review(w, "eth");
  input.chain.id = 8453;
  expect(w.network.chain.id).toBe(84532);
  expect(Object.isFrozen(w.network.chain)).toBe(true);
  expect(Object.isFrozen(w.network.chain.rpcUrls.default.http)).toBe(true);
  expect(() => {
    w.network.chain.id = 1;
  }).toThrow();
  expect(await w.sendPayment(r, send())).toMatch(/^0x[0-9a-f]{64}$/);
});

test("wrong unlock is distinguished from a network failure and does not sign or broadcast", async () => {
  const w = wallet(),
    r = await review(w),
    options = send();
  options.withSigner = async () => {
    throw new Error("private unlock detail");
  };
  const error = await w.sendPayment(r, options).catch((e) => e);
  expect(error.code).toBe("unlock_failed");
  expect(error.message).toContain("unlocked");
  expect(JSON.stringify(error)).not.toContain("private unlock detail");
  expect(options.reserveSubmission).not.toHaveBeenCalled();
  expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
});

// Application-owned wrappers must not erase a durable broadcast receipt.
test.each(["signer", "lock"] as const)(
  "%s cleanup after an acknowledged broadcast retains the expected receipt",
  async (wrapper) => {
    const w = wallet(),
      r = await review(w),
      options = send();
    if (wrapper === "signer")
      options.withSigner = async (run) => {
        await run(owner);
        throw new Error("private cleanup detail");
      };
    else
      options.withSpendLock = async (run) => {
        await run();
        throw new Error("private cleanup detail");
      };
    const error = await w.sendPayment(r, options).catch((e) => e);
    expect(error).toBeInstanceOf(BaseSubmissionUnknownError);
    expect(options.reserveSubmission).toHaveBeenCalledWith(error.submission);
    expect(error.submission.hash).toBe(keccak256(error.submission.serializedTransaction));
    expect(options.releaseSubmission).not.toHaveBeenCalled();
    expect(fixture.rpc.sendRawTransaction).toHaveBeenCalledOnce();
    expect(JSON.stringify(error)).not.toContain("private cleanup detail");
    expect(JSON.stringify(error)).not.toContain(error.submission.serializedTransaction);
    await expect(w.sendPayment(r, send())).rejects.toMatchObject({ code: "review_invalid" });
  },
);
test("a wrapper replacing a lost-ACK error still retains the saved receipt", async () => {
  const w = wallet(),
    r = await review(w),
    options = send();
  fixture.rpc.sendRawTransaction.mockRejectedValueOnce(new Error("ACK lost"));
  options.withSigner = async (run) => {
    try {
      return await run(owner);
    } catch {
      throw new Error("unlock wrapper replaced error");
    }
  };
  const error = await w.sendPayment(r, options).catch((e) => e);
  expect(error).toBeInstanceOf(BaseSubmissionUnknownError);
  expect(options.reserveSubmission).toHaveBeenCalledWith(error.submission);
  expect(options.releaseSubmission).not.toHaveBeenCalled();
  expect(fixture.rpc.sendRawTransaction).toHaveBeenCalledOnce();
});
test("malformed signer bytes are a transaction mismatch, not an unlock failure", async () => {
  const w = wallet(),
    r = await review(w),
    options = send();
  options.withSigner = async (run) =>
    run({ ...owner, signTransaction: async () => "0x02aabb" } as HDAccount);
  await expect(w.sendPayment(r, options)).rejects.toMatchObject({ code: "transaction_mismatch" });
  expect(options.reserveSubmission).not.toHaveBeenCalled();
  expect(fixture.rpc.sendRawTransaction).not.toHaveBeenCalled();
});
