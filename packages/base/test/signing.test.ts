import { expect, test, vi } from "vitest";
import { createPublicClient, custom, getAddress, type Address, type Hex } from "viem";
import { english, generateMnemonic, mnemonicToAccount } from "viem/accounts";
import {
  getUserOperationHash,
  getUserOperationTypedData,
  type UserOperation,
} from "viem/account-abstraction";
import { recoverAuthorizationAddress } from "viem/utils";
import { freezeTransferIntent } from "./test-bindings";
import { BASE_SMART_ACCOUNT as manifest } from "./test-bindings";
import { simple7702Owner } from "./test-bindings";
import {
  freezeSponsorPolicy,
  reviewedOperationHash,
  signJournaledAuthorization,
  signJournaledOperation,
  submitJournaledOperation,
  type AuthorizationRecord,
  type SaveRecord,
  type SignedOperationRecord,
  type SigningGuard,
} from "./test-bindings";
import type { SmartAccountReadiness } from "./test-bindings";

function setup() {
  const owner = mnemonicToAccount(generateMnemonic(english));
  const intent = freezeTransferIntent({
    owner: owner.address,
    recipient: "0x2222222222222222222222222222222222222222",
    amount: 1_234_567n,
    deadline: 1000,
  });
  const state: SmartAccountReadiness = Object.freeze({
    owner: owner.address,
    blockNumber: 100n,
    blockHash: `0x${"ab".repeat(32)}`,
    delegation: "empty",
    authorizationNonce: 17,
    operationNonce: 7n,
  });
  const guard: SigningGuard = {
    assertCurrent: vi.fn(),
    now: () => 500,
    recheck: vi.fn(async () => state),
  };
  const save = vi.fn<SaveRecord>(async () => {});
  const consent = {
    owner: owner.address,
    delegate: manifest.delegate,
    chainId: manifest.chainId,
    persistentDelegationApproved: true as const,
  };
  const sponsorPolicy = freezeSponsorPolicy({
    paymaster: "0x3333333333333333333333333333333333333333",
    maxFeePerGas: 1_000_000n,
    maxSponsoredCost: 470_000_000_000n,
  });
  const rpcRequest = vi.fn(async () => {
    throw new Error("Signing cannot make RPC calls.");
  });
  const client = createPublicClient({ transport: custom({ request: rpcRequest }) });
  const authorize = () =>
    signJournaledAuthorization({ owner, intent, state, consent, guard, save });
  return {
    owner,
    intent,
    state,
    guard,
    save,
    consent,
    client,
    rpcRequest,
    sponsorPolicy,
    authorize,
  };
}
function operation(
  context: ReturnType<typeof setup>,
  authorization: Awaited<ReturnType<typeof signJournaledAuthorization>>,
): UserOperation<"0.8"> {
  return {
    sender: context.owner.address,
    nonce: 7n,
    callData: context.intent.callData,
    factory: "0x7702",
    factoryData: "0x",
    authorization: authorization.authorization,
    signature: "0x",
    callGasLimit: 100_000n,
    verificationGasLimit: 100_000n,
    preVerificationGas: 70_000n,
    maxFeePerGas: 1_000_000n,
    maxPriorityFeePerGas: 100_000n,
    paymaster: "0x3333333333333333333333333333333333333333",
    paymasterData: "0x1234",
    paymasterVerificationGasLimit: 100_000n,
    paymasterPostOpGasLimit: 100_000n,
  };
}
async function signed(
  context: ReturnType<typeof setup>,
  mutation: Partial<UserOperation<"0.8">> = {},
) {
  const authorization = await context.authorize();
  const prepared = { ...operation(context, authorization), ...mutation };
  // The normal helper approves the final unchanged sponsor quote. Mutation tests exercise validation first.
  return signJournaledOperation({
    ...context,
    authorization,
    prepared,
    consent: {
      callData: context.intent.callData,
      deadline: context.intent.deadline,
      operationHash: reviewedOperationHash(operation(context, authorization)),
      sponsoredFeesApproved: true,
    },
  });
}

test("forwards the existing HD account without extracting a private key", async () => {
  const c = setup();
  vi.spyOn(c.owner, "getHdKey").mockImplementation(() => {
    throw new Error("No key extraction");
  });
  const facade = simple7702Owner(c.owner);
  expect(facade.address).toBe(c.owner.address);
  const result = await c.authorize();
  expect(
    getAddress(await recoverAuthorizationAddress({ authorization: result.authorization })),
  ).toBe(c.owner.address);
  expect(result.authorization).toMatchObject({
    chainId: 8453,
    nonce: 17,
    address: manifest.delegate,
  });
  expect(c.save.mock.calls[0][0].kind).toBe("authorization");
  expect(Object.isFrozen(result.authorization)).toBe(true);
});

test("requires explicit persistent delegation consent before signing", async () => {
  const c = setup();
  const spy = vi.spyOn(c.owner, "signAuthorization");
  await expect(
    signJournaledAuthorization({ ...c, consent: { ...c.consent, chainId: 1 } }),
  ).rejects.toThrow("Approve the pinned");
  expect(spy).not.toHaveBeenCalled();
});

test("journals exact crypto hashes and frozen raw request before the one raw submission", async () => {
  const c = setup();
  const handle = await signed(c);
  expect(c.save).toHaveBeenCalledTimes(2);
  const record = c.save.mock.calls[1][0] as SignedOperationRecord;
  expect(handle.expectedHash).toBe(
    getUserOperationHash({
      chainId: 8453,
      entryPointAddress: manifest.entryPoint,
      entryPointVersion: "0.8",
      userOperation: record.operation,
    }),
  );
  expect(Object.isFrozen(handle.request)).toBe(true);
  expect(Object.isFrozen(handle.request.eip7702Auth)).toBe(true);
  expect(handle.request.factory).toBe("0x7702");
  expect(handle.request.eip7702Auth?.chainId).toBe("0x2105");
  const request = vi.fn(async () => {
    expect(c.save).toHaveBeenCalledTimes(2);
    return handle.expectedHash;
  });
  await expect(submitJournaledOperation(handle, { request }, c.guard)).resolves.toBe(
    handle.expectedHash,
  );
  expect(request).toHaveBeenCalledExactlyOnceWith({
    method: "eth_sendUserOperation",
    params: [handle.request, manifest.entryPoint],
  });
  await expect(submitJournaledOperation(handle, { request }, c.guard)).rejects.toThrow(
    "already been submitted",
  );
  expect(request).toHaveBeenCalledTimes(1);
});

test("the 7702 sentinel binds the pinned delegate into typed operation hashing", async () => {
  const c = setup();
  await signed(c);
  const record = c.save.mock.calls[1][0] as SignedOperationRecord;
  const hash = (op: UserOperation<"0.8">) =>
    getUserOperationHash({
      chainId: 8453,
      entryPointAddress: manifest.entryPoint,
      entryPointVersion: "0.8",
      userOperation: op,
    });
  const altered = {
    ...record.operation,
    authorization: { ...record.operation.authorization!, address: c.intent.recipient },
  };
  expect(hash(altered)).not.toBe(record.expectedHash);
  expect(hash({ ...altered, factory: undefined })).toBe(
    hash({ ...record.operation, factory: undefined }),
  );
  expect(
    getUserOperationTypedData({
      chainId: 8453,
      entryPointAddress: manifest.entryPoint,
      userOperation: record.operation,
    }).domain?.chainId,
  ).toBe(8453);
});

test.each([
  { sender: "0x4444444444444444444444444444444444444444" as Address },
  { nonce: 8n },
  { factory: undefined },
  { factoryData: "0x12" as Hex },
  { paymaster: undefined },
  { paymasterData: undefined },
  { maxFeePerGas: 0n },
  { callData: "0x" as Hex },
  { signature: "0x12" as Hex },
])("rejects mutated provider fields before signing %o", async (mutation) => {
  const c = setup();
  const spy = vi.spyOn(c.owner, "signTypedData");
  await expect(signed(c, mutation)).rejects.toThrow();
  expect(spy).not.toHaveBeenCalled();
  expect(c.save).toHaveBeenCalledTimes(1);
});

test("a provider cannot substitute the authorization signature or chain", async () => {
  const c = setup();
  const auth = await c.authorize();
  await expect(
    signJournaledOperation({
      ...c,
      authorization: auth,
      prepared: { ...operation(c, auth), authorization: { ...auth.authorization, chainId: 0 } },
      consent: {
        callData: c.intent.callData,
        deadline: 1000,
        operationHash: reviewedOperationHash(operation(c, auth)),
        sponsoredFeesApproved: true,
      },
    }),
  ).rejects.toThrow("changed the saved Base authorization");
});

test.each(["owner", "authorizationNonce", "operationNonce", "delegation"] as const)(
  "stale %s prevents authorization disclosure",
  async (field) => {
    const c = setup();
    const stale = {
      ...c.state,
      [field]:
        field === "owner"
          ? c.intent.recipient
          : field === "delegation"
            ? "pinned"
            : field === "operationNonce"
              ? 8n
              : 18,
    };
    c.guard.recheck = vi.fn(async () => stale as SmartAccountReadiness);
    await expect(c.authorize()).rejects.toThrow("state changed");
    expect(c.save).not.toHaveBeenCalled();
  },
);

test("failed durable authorization save never returns a disclosure handle", async () => {
  const c = setup();
  c.save.mockRejectedValue(new Error("Disk full"));
  await expect(c.authorize()).rejects.toThrow("Disk full");
});

test("failed operation save never returns a submission handle", async () => {
  const c = setup();
  c.save.mockImplementation(async (r) => {
    if (r.kind === "signed-operation") throw new Error("Disk full");
  });
  await expect(signed(c)).rejects.toThrow("Disk full");
  expect(c.save).toHaveBeenCalledTimes(2);
});

test("cancellation after durable save still prevents authorization disclosure", async () => {
  const c = setup();
  let cancelled = false;
  c.guard.assertCurrent = () => {
    if (cancelled) throw new Error("Cancelled");
  };
  c.save.mockImplementation(async () => {
    cancelled = true;
  });
  await expect(c.authorize()).rejects.toThrow("Cancelled");
  expect(c.save.mock.calls[0][0].kind).toBe("authorization");
});

test("expiry prevents signing and submission", async () => {
  const c = setup();
  c.guard.now = () => 1000;
  await expect(c.authorize()).rejects.toThrow("expired");
  const next = setup();
  const handle = await signed(next);
  next.guard.now = () => 1000;
  const request = vi.fn(async () => handle.expectedHash);
  await expect(submitJournaledOperation(handle, { request }, next.guard)).rejects.toThrow(
    "expired",
  );
  expect(request).not.toHaveBeenCalled();
});

test.each(["lost acknowledgement", "hash mismatch"])(
  "%s preserves the hash and forbids a retry",
  async (failure) => {
    const c = setup();
    const handle = await signed(c);
    const request = vi.fn(async () => {
      if (failure === "lost acknowledgement") throw new Error("Network lost");
      return `0x${"00".repeat(32)}`;
    });
    await expect(submitJournaledOperation(handle, { request }, c.guard)).rejects.toThrow();
    await expect(submitJournaledOperation(handle, { request }, c.guard)).rejects.toThrow(
      "already been submitted",
    );
    expect(request).toHaveBeenCalledTimes(1);
    expect((c.save.mock.calls[1][0] as SignedOperationRecord).expectedHash).toBe(
      handle.expectedHash,
    );
  },
);

test("concurrent submission calls only disclose the saved operation once", async () => {
  const c = setup();
  const handle = await signed(c);
  const request = vi.fn(async () => handle.expectedHash);
  const results = await Promise.allSettled([
    submitJournaledOperation(handle, { request }, c.guard),
    submitJournaledOperation(handle, { request }, c.guard),
  ]);
  expect(results.filter((r) => r.status === "fulfilled")).toHaveLength(1);
  expect(request).toHaveBeenCalledTimes(1);
});
test("failed durable submission preparation can retry the same handle, with concurrent attempts excluded", async () => {
  const c = setup(), handle = await signed(c);
  const request = vi.fn(async () => handle.expectedHash);
  let release!: () => void;
  const held = new Promise<void>(resolve => { release = resolve; });
  const preparing = submitJournaledOperation(handle, { request }, c.guard, async () => {
    await held;
    throw new Error("journal unavailable");
  });
  await expect(submitJournaledOperation(handle, { request }, c.guard)).rejects.toThrow("already been submitted");
  release();
  await expect(preparing).rejects.toThrow("journal unavailable");
  expect(request).not.toHaveBeenCalled();
  await expect(submitJournaledOperation(handle, { request }, c.guard)).resolves.toBe(handle.expectedHash);
  expect(request).toHaveBeenCalledTimes(1);
  expect(c.save).toHaveBeenCalledTimes(2);
});

test("saved authorization is not recreated or automatically sent by signing", async () => {
  const c = setup();
  const handle = await signed(c);
  const auth = c.save.mock.calls[0][0] as AuthorizationRecord;
  expect(auth.authorization.nonce).toBe(17);
  expect(handle.expectedHash).toMatch(/^0x[0-9a-f]{64}$/);
  expect(c.rpcRequest).not.toHaveBeenCalled();
});

test.each(["chain", "nonce", "delegate", "signer"])(
  "rejects a signer returning the wrong authorization %s",
  async (mutation) => {
    const c = setup();
    const signAuthorization = c.owner.signAuthorization!;
    const other = mnemonicToAccount(generateMnemonic(english));
    vi.spyOn(c.owner, "signAuthorization").mockImplementation(async (parameters) => {
      if (mutation === "signer") return other.signAuthorization!(parameters);
      return signAuthorization({
        address: mutation === "delegate" ? c.intent.recipient : manifest.delegate,
        chainId: mutation === "chain" ? 0 : 8453,
        nonce: mutation === "nonce" ? 18 : 17,
      });
    });
    await expect(c.authorize()).rejects.toThrow(
      "authorization signer, nonce, chain or implementation mismatch",
    );
    expect(c.save).not.toHaveBeenCalled();
  },
);

test("an incorrect typed operation signature cannot be saved or submitted", async () => {
  const c = setup();
  const other = mnemonicToAccount(generateMnemonic(english));
  vi.spyOn(c.owner, "signTypedData").mockImplementation((parameters) =>
    other.signTypedData(parameters),
  );
  await expect(signed(c)).rejects.toThrow("signature is not from the reviewed wallet");
  expect(c.save).toHaveBeenCalledTimes(1);
});

test("cancellation after an operation save prevents submission disclosure", async () => {
  const c = setup();
  let cancelled = false;
  c.guard.assertCurrent = () => {
    if (cancelled) throw new Error("Cancelled");
  };
  c.save.mockImplementation(async (record) => {
    if (record.kind === "signed-operation") cancelled = true;
  });
  await expect(signed(c)).rejects.toThrow("Cancelled");
  expect(c.save.mock.calls[1][0].kind).toBe("signed-operation");
});

test("requires an explicitly frozen sponsor policy", async () => {
  const c = setup();
  c.sponsorPolicy = { ...c.sponsorPolicy };
  const spy = vi.spyOn(c.owner, "signTypedData");
  await expect(signed(c)).rejects.toThrow("explicit frozen Base sponsor policy");
  expect(spy).not.toHaveBeenCalled();
  expect(c.save).toHaveBeenCalledTimes(1);
});

test("rejects an unapproved paymaster even when its quote is within budget", async () => {
  const c = setup();
  const spy = vi.spyOn(c.owner, "signTypedData");
  await expect(signed(c, { paymaster: c.intent.recipient })).rejects.toThrow(
    "paymaster is not allowed",
  );
  expect(spy).not.toHaveBeenCalled();
});

test("rejects a fee per gas above the explicit sponsor cap", async () => {
  const c = setup();
  await expect(signed(c, { maxFeePerGas: c.sponsorPolicy.maxFeePerGas + 1n })).rejects.toThrow(
    "gas fee exceeds policy",
  );
  expect(c.save).toHaveBeenCalledTimes(1);
});

test.each([
  "callGasLimit",
  "verificationGasLimit",
  "preVerificationGas",
  "paymasterVerificationGasLimit",
  "paymasterPostOpGasLimit",
] as const)("counts %s in the total sponsored budget", async (field) => {
  const c = setup();
  const authorization = await c.authorize();
  const original = operation(c, authorization);
  const prepared = { ...original, [field]: original[field]! + 1n };
  const spy = vi.spyOn(c.owner, "signTypedData");
  await expect(
    signJournaledOperation({
      ...c,
      authorization,
      prepared,
      consent: {
        callData: c.intent.callData,
        deadline: c.intent.deadline,
        operationHash: reviewedOperationHash(prepared),
        sponsoredFeesApproved: true,
      },
    }),
  ).rejects.toThrow("total Base sponsored cost exceeds policy");
  expect(spy).not.toHaveBeenCalled();
  expect(c.save).toHaveBeenCalledTimes(1);
});

test.each([
  { callGasLimit: 100_001n },
  { verificationGasLimit: 100_001n },
  { preVerificationGas: 70_001n },
  { paymasterVerificationGasLimit: 100_001n },
  { paymasterPostOpGasLimit: 100_001n },
  { maxFeePerGas: 999_999n },
  { maxPriorityFeePerGas: 99_999n },
  { paymasterData: "0x5678" as Hex },
])(
  "rejects a changed sponsor quote after approval despite being within policy %o",
  async (mutation) => {
    const c = setup();
    c.sponsorPolicy = freezeSponsorPolicy({
      ...c.sponsorPolicy,
      maxSponsoredCost: c.sponsorPolicy.maxSponsoredCost * 2n,
    });
    const spy = vi.spyOn(c.owner, "signTypedData");
    await expect(signed(c, mutation)).rejects.toThrow(
      "Confirm the exact reviewed sponsored USDC transfer",
    );
    expect(spy).not.toHaveBeenCalled();
    expect(c.save).toHaveBeenCalledTimes(1);
  },
);

test("freezes the approved quote before awaiting wallet state rechecks", async () => {
  const c = setup();
  const authorization = await c.authorize();
  const prepared = operation(c, authorization);
  const operationHash = reviewedOperationHash(prepared);
  c.guard.recheck = async () => {
    prepared.maxFeePerGas = 999_999n;
    prepared.paymasterData = "0x5678";
    return c.state;
  };
  const handle = await signJournaledOperation({
    ...c,
    authorization,
    prepared,
    consent: {
      callData: c.intent.callData,
      deadline: c.intent.deadline,
      operationHash,
      sponsoredFeesApproved: true,
    },
  });
  expect(handle.expectedHash).toBe(operationHash);
  const record = c.save.mock.calls[1][0] as SignedOperationRecord;
  expect(record.operation.maxFeePerGas).toBe(1_000_000n);
  expect(record.operation.paymasterData).toBe("0x1234");
});

test.each([
  {
    paymaster: "0x0000000000000000000000000000000000000000" as Address,
    maxFeePerGas: 1n,
    maxSponsoredCost: 1n,
  },
  {
    paymaster: "0x3333333333333333333333333333333333333333" as Address,
    maxFeePerGas: 0n,
    maxSponsoredCost: 1n,
  },
  {
    paymaster: "0x3333333333333333333333333333333333333333" as Address,
    maxFeePerGas: 1n,
    maxSponsoredCost: 0n,
  },
])("rejects invalid explicit sponsor policy %o", (policy) => {
  expect(() => freezeSponsorPolicy(policy)).toThrow("Invalid explicit Base sponsor policy");
});
