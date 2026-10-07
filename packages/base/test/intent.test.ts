import { expect, test } from "vitest";
import { encodeFunctionData, parseAbi, type Hex } from "viem";
import { assertTransferIntent, freezeTransferIntent } from "./test-bindings";
import { accountAbi, BASE_SMART_ACCOUNT as manifest, usdcTransferAbi } from "./test-bindings";
const intent = freezeTransferIntent({
  owner: "0x1111111111111111111111111111111111111111",
  recipient: "0x2222222222222222222222222222222222222222",
  amount: 1_234_567n,
  deadline: 1000,
});

test("freezes the exact single Base USDC transfer", () => {
  expect(Object.isFrozen(intent)).toBe(true);
  expect(() => assertTransferIntent(intent, intent.callData)).not.toThrow();
});
test.each([
  encodeFunctionData({ abi: accountAbi, functionName: "execute", args: [manifest.usdc, 1n, "0x"] }),
  encodeFunctionData({
    abi: accountAbi,
    functionName: "execute",
    args: [intent.recipient, 0n, "0x"],
  }),
  encodeFunctionData({
    abi: accountAbi,
    functionName: "execute",
    args: [
      manifest.usdc,
      0n,
      encodeFunctionData({
        abi: usdcTransferAbi,
        functionName: "transfer",
        args: [intent.recipient, intent.amount + 1n],
      }),
    ],
  }),
  encodeFunctionData({
    abi: accountAbi,
    functionName: "execute",
    args: [
      manifest.usdc,
      0n,
      encodeFunctionData({
        abi: parseAbi(["function approve(address spender,uint256 amount) returns (bool)"]),
        functionName: "approve",
        args: [intent.recipient, 1n << 255n],
      }),
    ],
  }),
  `${intent.callData}00` as Hex,
  "0x" as Hex,
])("rejects changed intent or unsupported execution", (data) => {
  expect(() => assertTransferIntent(intent, data)).toThrow();
});
test.each([0n, -1n, 1n << 256n])("rejects invalid transfer amount %s", (amount) => {
  expect(() => freezeTransferIntent({ ...intent, amount })).toThrow();
});
