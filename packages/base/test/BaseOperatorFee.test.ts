import { afterEach, expect, test, vi } from "vitest";
import { createPublicClient, encodeFunctionData, http, parseAbi } from "viem";
import { base } from "viem/chains";
import { publicActionsL2 } from "viem/op-stack";
import { estimateBaseOperatorFee } from "./test-bindings";

afterEach(() => vi.restoreAllMocks());
const GAS = 25_202n;
function client(
  response: (request: { id: number; method: string; params: unknown[] }) => Response,
) {
  const fetchFn = vi.fn(async (_url, options) => response(JSON.parse(String(options.body))));
  const rpc = createPublicClient({
    chain: base,
    transport: http("https://public-base-rpc.test", { retryCount: 0, fetchFn }),
  }).extend(publicActionsL2());
  return { rpc, fetchFn };
}
function result(id: number, value: string) {
  return Response.json({ jsonrpc: "2.0", id, result: value });
}

test.each([0n, 75_606_007n])("accepts confirmed on-chain operator fee %s", async (fee) => {
  const { rpc, fetchFn } = client(({ id }) =>
    result(id, `0x${fee.toString(16).padStart(64, "0")}`),
  );
  expect(await estimateBaseOperatorFee(rpc, GAS)).toBe(fee);
  expect(fetchFn).toHaveBeenCalledTimes(1);
  const request = JSON.parse(String(fetchFn.mock.calls[0][1].body));
  expect(request.method).toBe("eth_call");
  expect(request.params[0].to.toLowerCase()).toBe("0x420000000000000000000000000000000000000f");
  expect(request.params[0].data).toBe(
    encodeFunctionData({
      abi: parseAbi(["function getOperatorFee(uint256) view returns (uint256)"]),
      functionName: "getOperatorFee",
      args: [GAS],
    }),
  );
  expect(request.params[1]).toBe("latest");
});

test.each([429, 503])("HTTP %s cannot silently omit the operator fee", async (status) => {
  const { rpc, fetchFn } = client(() => new Response("Unavailable", { status }));
  await expect(estimateBaseOperatorFee(rpc, GAS)).rejects.toThrow("network is unavailable");
  expect(fetchFn).toHaveBeenCalledTimes(1);
});
test("RPC revert and absent oracle method fail closed", async () => {
  const { rpc } = client(({ id }) =>
    Response.json({ jsonrpc: "2.0", id, error: { code: 3, message: "execution reverted" } }),
  );
  await expect(estimateBaseOperatorFee(rpc, GAS)).rejects.toThrow("network is unavailable");
});
test("malformed ABI response cannot become a zero operator fee", async () => {
  const { rpc } = client(({ id }) => result(id, "0x"));
  await expect(estimateBaseOperatorFee(rpc, GAS)).rejects.toThrow("network is unavailable");
});
test("transport rejection fails closed", async () => {
  const { rpc } = client(() => {
    throw new TypeError("Network unavailable");
  });
  await expect(estimateBaseOperatorFee(rpc, GAS)).rejects.toThrow("network is unavailable");
});
test("pinned legacy estimator reproduces the rate-limit omission, strict oracle rejects it", async () => {
  const { rpc } = client(() => new Response("Unavailable", { status: 429 }));
  expect(
    await rpc.estimateOperatorFee({
      account: "0x1111111111111111111111111111111111111111",
      to: "0x2222222222222222222222222222222222222222",
      value: 1n,
    }),
  ).toBe(0n);
  await expect(estimateBaseOperatorFee(rpc, GAS)).rejects.toThrow("network is unavailable");
});
test.each([0n, -1n, 1n << 64n])("invalid gas %s makes no oracle request", async (gas) => {
  const readContract = vi.fn();
  await expect(estimateBaseOperatorFee({ readContract }, gas)).rejects.toThrow(
    "invalid fee estimates",
  );
  expect(readContract).not.toHaveBeenCalled();
});
test.each([-1n, "0", 1n << 256n])("invalid decoded fee %s fails closed", async (fee) => {
  await expect(
    estimateBaseOperatorFee({ readContract: vi.fn().mockResolvedValue(fee) }, GAS),
  ).rejects.toThrow("invalid fee estimates");
});
