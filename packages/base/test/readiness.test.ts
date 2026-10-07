import { expect, test, vi } from "vitest";
import {
  createPublicClient,
  encodeAbiParameters,
  http,
  keccak256,
  type Address,
  type Hex,
} from "viem";
import { BASE_SMART_ACCOUNT as manifest, delegationCode } from "./test-bindings";
import { readSmartAccountReadiness } from "./test-bindings";
import delegateFixture from "./fixtures/delegate.json";
import entryPointFixture from "./fixtures/entryPoint.json";

const owner: Address = "0x1111111111111111111111111111111111111111";
const blockHash = `0x${"ab".repeat(32)}`;
function rpc(
  options: {
    code?: Hex;
    chainId?: string;
    delegate?: Hex;
    entryPoint?: Hex;
    configured?: Address;
    nonce?: bigint;
    canonicalChanges?: boolean;
    authorizationNonce?: number;
    pendingAuthorizationNonce?: number;
  } = {},
) {
  const fetchFn = vi.fn(async (_url, init) => {
    const request = JSON.parse(String(init.body));
    let result: unknown;
    switch (request.method) {
      case "eth_chainId":
        result = options.chainId ?? "0x2105";
        break;
      case "eth_getBlockByNumber":
        result = {
          number: "0x123",
          hash:
            options.canonicalChanges && request.params[0] !== "latest"
              ? `0x${"cd".repeat(32)}`
              : blockHash,
          transactions: [],
        };
        break;
      case "eth_getCode": {
        const address = request.params[0].toLowerCase();
        result =
          address === manifest.delegate.toLowerCase()
            ? (options.delegate ?? delegateFixture.code)
            : address === manifest.entryPoint.toLowerCase()
              ? (options.entryPoint ?? entryPointFixture.code)
              : (options.code ?? "0x");
        break;
      }
      case "eth_call":
        result =
          request.params[0].to.toLowerCase() === manifest.delegate.toLowerCase()
            ? encodeAbiParameters(
                [{ type: "address" }],
                [options.configured ?? manifest.entryPoint],
              )
            : encodeAbiParameters([{ type: "uint256" }], [options.nonce ?? 7n]);
        break;
      case "eth_getTransactionCount":
        result = `0x${(request.params[1] === "pending" ? (options.pendingAuthorizationNonce ?? options.authorizationNonce ?? 9) : (options.authorizationNonce ?? 9)).toString(16)}`;
        break;
      default:
        throw new Error(`Unexpected ${request.method}`);
    }
    return Response.json({ jsonrpc: "2.0", id: request.id, result });
  });
  return {
    client: createPublicClient({
      transport: http("https://intercepted.test", { fetchFn, retryCount: 0 }),
    }),
    fetchFn,
  };
}

test("checked-in public runtime fixtures match the pinned manifest", () => {
  expect(keccak256(delegateFixture.code as Hex)).toBe(manifest.delegateCodeHash);
  expect(keccak256(entryPointFixture.code as Hex)).toBe(manifest.entryPointCodeHash);
  expect(manifest.enabled).toBe(false);
});

test.each(["0x", delegationCode] as const)(
  "restores exact owner delegation %s and zero nonce lane from canonical reads",
  async (code) => {
    const { client, fetchFn } = rpc({ code });
    const result = await readSmartAccountReadiness(client, owner);
    expect(result).toMatchObject({
      owner,
      blockNumber: 291n,
      blockHash,
      delegation: code === "0x" ? "empty" : "pinned",
      authorizationNonce: 9,
      operationNonce: 7n,
    });
    const requests = fetchFn.mock.calls.map(([, init]) => JSON.parse(String(init.body)));
    expect(
      requests
        .filter((r) => ["eth_getCode", "eth_call"].includes(r.method))
        .every((r) => r.params[1] === "0x123"),
    ).toBe(true);
    expect(
      requests.filter((r) => r.method === "eth_getTransactionCount").map((r) => r.params[1]),
    ).toEqual(["0x123", "pending"]);
    const nonceCall = requests.find(
      (r) =>
        r.method === "eth_call" &&
        r.params[0].to.toLowerCase() === manifest.entryPoint.toLowerCase(),
    );
    expect(nonceCall.params[0].data.endsWith("0".repeat(64))).toBe(true);
  },
);

test.each([
  [{ pendingAuthorizationNonce: 10 }, "nonce is pending"],
  [{ pendingAuthorizationNonce: 8 }, "nonce is pending"],
  [{ chainId: "0x1" }, "chain ID"],
  [{ delegate: "0x6000" }, "implementation code"],
  [{ entryPoint: "0x6000" }, "EntryPoint code"],
  [{ configured: owner }, "EntryPoint mismatch"],
  [{ code: "0x6000" }, "unknown Base account code"],
  [{ code: `${delegationCode}00` }, "unknown Base account code"],
  [{ nonce: 1n << 64n }, "fixed zero lane"],
  [{ canonicalChanges: true }, "canonical state changed"],
] as const)("fails closed for invalid readiness %o", async (options, message) => {
  await expect(readSmartAccountReadiness(rpc(options).client, owner)).rejects.toThrow(message);
});

test("a previously pinned owner is checked again after a code upgrade or revocation", async () => {
  await expect(
    readSmartAccountReadiness(rpc({ code: delegationCode }).client, owner),
  ).resolves.toMatchObject({ delegation: "pinned" });
  await expect(readSmartAccountReadiness(rpc({ code: "0x6000" }).client, owner)).rejects.toThrow(
    "unknown Base account code",
  );
  await expect(readSmartAccountReadiness(rpc({ code: "0x" }).client, owner)).resolves.toMatchObject(
    { delegation: "empty" },
  );
});
