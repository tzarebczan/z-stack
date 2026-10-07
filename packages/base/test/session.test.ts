import { expect, test, vi } from "vitest";
import { createPublicClient, custom, type Hash } from "viem";
import type { UserOperation } from "viem/account-abstraction";
import { deriveBaseAccount } from "../src/index";
import {
  createBaseSmartAccount,
  runBaseSponsoredTransfer,
  createBaseBundlerTransport,
  type JournaledOperation,
  type SigningGuard,
} from "../src/smart-account/index";
import { BASE_SMART_ACCOUNT as manifest } from "./test-bindings";
function setup() {
  const smart = createBaseSmartAccount(manifest),
    owner = deriveBaseAccount("test test test test test test test test test test test junk");
  const intent = smart.freezeTransferIntent({
    owner: owner.address,
    recipient: "0x2222222222222222222222222222222222222222",
    amount: 1_000_000n,
    deadline: 1000,
  });
  const state = {
    owner: owner.address,
    blockNumber: 100n,
    blockHash: `0x${"ab".repeat(32)}` as Hash,
    delegation: "empty" as const,
    authorizationNonce: 0,
    operationNonce: 0n,
  };
  const events: string[] = [];
  const guard: SigningGuard = {
    assertCurrent: vi.fn(),
    now: () => 500,
    recheck: async () => state,
  };
  const policy = smart.freezeSponsorPolicy({
    paymaster: "0x3333333333333333333333333333333333333333",
    maxFeePerGas: 1_000_000n,
    maxSponsoredCost: 1_000_000_000_000n,
  });
  const client = createPublicClient({
    transport: custom({
      request: async () => {
        throw new Error("No RPC signing calls");
      },
    }),
  });
  client.estimateFeesPerGas = async () => ({ maxFeePerGas: 10n, maxPriorityFeePerGas: 1n });
  const approvals = {
    approveCode: vi.fn(async (details) => ({
      owner: details.owner,
      delegate: details.delegate,
      chainId: details.chainId,
      persistentDelegationApproved: true as const,
    })),
    approveTransfer: vi.fn(async (details) => ({
      callData: details.intent.callData,
      deadline: details.intent.deadline,
      operationHash: details.operationHash,
      sponsoredFeesApproved: true as const,
    })),
  };
  const journal = {
    save: vi.fn(async (record) => {
      events.push(record.kind);
    }),
    setSubmission: vi.fn(async (value) => {
      events.push(value.phase);
    }),
  };
  const provider = {
    quote: vi.fn(async (operation: UserOperation<"0.8">) => {
      events.push("quote");
      return {
        ...operation,
        paymaster: policy.paymaster,
        paymasterData: "0x" as const,
        paymasterVerificationGasLimit: 50_000n,
        paymasterPostOpGasLimit: 20_000n,
      };
    }),
    bundler: {
      request: vi.fn(async (request) => {
        events.push("send");
        return journal.setSubmission.mock.calls.at(-1)?.[0].hash;
      }),
    },
  };
  return {
    account: smart,
    client,
    owner,
    intent,
    state,
    guard,
    policy,
    approvals,
    provider,
    journal,
    events,
  };
}
test("own paymaster uses separate approvals and durable authorization/operation/unknown writes before one send", async () => {
  const c = setup(),
    result = await runBaseSponsoredTransfer(c);
  expect(c.events).toEqual([
    "authorization",
    "quote",
    "signed-operation",
    "submission-unknown",
    "send",
    "submitted",
  ]);
  expect(result.phase).toBe("submitted");
  expect(c.provider.bundler.request).toHaveBeenCalledTimes(1);
  expect(c.approvals.approveTransfer).toHaveBeenCalledWith(
    expect.objectContaining({
      userPaysUsdc: 0n,
      delayedExecutionPossible: true,
      submissionDeadlineOnly: true,
    }),
  );
});
test.each(["paymaster", "callData", "fee"])(
  "malicious quote %s cannot obtain an operation signature or submission",
  async (kind) => {
    const c = setup(),
      quote = c.provider.quote.getMockImplementation()!;
    c.provider.quote.mockImplementation(async (op) => {
      const result = await quote(op);
      return kind === "paymaster"
        ? { ...result, paymaster: c.intent.recipient }
        : kind === "callData"
          ? { ...result, callData: "0x1234" }
          : { ...result, maxFeePerGas: c.policy.maxFeePerGas + 1n };
    });
    await expect(runBaseSponsoredTransfer(c)).rejects.toThrow();
    expect(c.events).toEqual(["authorization", "quote"]);
    expect(c.approvals.approveTransfer).not.toHaveBeenCalled();
    expect(c.provider.bundler.request).not.toHaveBeenCalled();
  },
);
test("failed unknown journal write never calls the bundler", async () => {
  const c = setup();
  c.journal.setSubmission.mockRejectedValue(new Error("disk full"));
  await expect(runBaseSponsoredTransfer(c)).rejects.toThrow("disk full");
  expect(c.provider.bundler.request).not.toHaveBeenCalled();
});
test.each(["expired", "cancelled", "readiness"])(
  "a %s final guard leaves no unknown marker and keeps the same signed operation retryable",
  async failure => {
    const c = setup();
    const submit = c.account.submitJournaledOperation;
    let handle!: JournaledOperation, checking = false, checks = 0, failed = false;
    c.guard.recheck = async () => {
      if (checking && ++checks === 2) {
        failed = true;
        if (failure === "readiness") return { ...c.state, operationNonce: 1n };
      }
      return c.state;
    };
    c.guard.now = () => failure === "expired" && failed ? 1000 : 500;
    c.guard.assertCurrent = () => { if (failure === "cancelled" && failed) throw new Error("Cancelled"); };
    c.account = {
      ...c.account,
      submitJournaledOperation: async (...args) => { handle = args[0]; checking = true; return submit(...args); },
    };
    await expect(runBaseSponsoredTransfer(c)).rejects.toThrow();
    expect(c.events).toEqual(["authorization", "quote", "signed-operation"]);
    expect(c.journal.setSubmission).not.toHaveBeenCalled();
    expect(c.provider.bundler.request).not.toHaveBeenCalled();
    failed = checking = false;
    await expect(submit(handle, c.provider.bundler, c.guard, () => c.journal.setSubmission({
      hash: handle.expectedHash, phase: "submission-unknown",
    }))).resolves.toBe(handle.expectedHash);
    expect(c.provider.bundler.request).toHaveBeenCalledTimes(1);
    expect(c.journal.save).toHaveBeenCalledTimes(2); // No second authorization or operation signature.
  },
);
test("a deadline crossing during the dispatch marker does not strand an unsent operation", async () => {
  const c = setup();
  const write = c.journal.setSubmission.getMockImplementation()!;
  c.journal.setSubmission.mockImplementation(async value => {
    await write(value);
    if (value.phase === "submission-unknown") c.guard.now = () => 1000;
  });
  await expect(runBaseSponsoredTransfer(c)).resolves.toMatchObject({ phase: "submitted" });
  expect(c.provider.bundler.request).toHaveBeenCalledTimes(1);
});
test("lost bundler acknowledgement keeps the saved unknown hash and never retries", async () => {
  const c = setup();
  c.provider.bundler.request.mockRejectedValue(new Error("lost"));
  await expect(runBaseSponsoredTransfer(c)).rejects.toThrow("lost");
  expect(c.journal.setSubmission).toHaveBeenCalledWith(
    expect.objectContaining({ phase: "submission-unknown" }),
  );
  expect(c.provider.bundler.request).toHaveBeenCalledTimes(1);
});
test("deployment is snapshotted and authorization handles cannot cross instances", async () => {
  const c = setup(),
    input = { ...manifest },
    kit = createBaseSmartAccount(input);
  input.usdc = c.intent.recipient;
  expect(kit.deployment.usdc).toBe(manifest.usdc);
  expect(Object.isFrozen(kit.deployment)).toBe(true);
  const authorization = await c.account.signJournaledAuthorization({
    ...c,
    consent: {
      owner: c.owner.address,
      delegate: manifest.delegate,
      chainId: 8453,
      persistentDelegationApproved: true,
    },
    save: c.journal.save,
  });
  await expect(
    kit.signJournaledOperation({
      ...c,
      authorization,
      prepared: await c.provider.quote({} as UserOperation<"0.8">),
      consent: {} as never,
      sponsorPolicy: c.policy,
      save: c.journal.save,
    }),
  ).rejects.toThrow("Save the Base authorization");
});
test("raw bundler sends one request without ambient credentials or redirects", async () => {
  const request = vi.fn(
    async () =>
      new Response(JSON.stringify({ jsonrpc: "2.0", id: 1, result: "0x" + "a".repeat(64) }), {
        headers: { "Content-Type": "application/json" },
      }),
  );
  const transport = createBaseBundlerTransport({ url: "https://example.com/bundler", request });
  await transport.request({
    method: "eth_sendUserOperation",
    params: [{} as never, manifest.entryPoint],
  });
  expect(request).toHaveBeenCalledTimes(1);
  expect(request).toHaveBeenCalledWith(
    "https://example.com/bundler",
    expect.objectContaining({ credentials: "omit", redirect: "error", cache: "no-store" }),
  );
});
test("HTTP failure and oversized bundler responses cannot retry", async () => {
  for (const response of [new Response("", { status: 503 }), new Response("x".repeat(32769))]) {
    const request = vi.fn(async () => response),
      transport = createBaseBundlerTransport({ url: "https://example.com", request });
    await expect(
      transport.request({
        method: "eth_sendUserOperation",
        params: [{} as never, manifest.entryPoint],
      }),
    ).rejects.toThrow();
    expect(request).toHaveBeenCalledTimes(1);
  }
});

test("rejects a custom EntryPoint that viem Simple7702 would not sign for", () => {
  expect(() =>
    createBaseSmartAccount({
      ...manifest,
      entryPoint: "0x2222222222222222222222222222222222222222",
    }),
  ).toThrow("canonical EntryPoint 0.8");
});
