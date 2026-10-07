import { parseAbi } from "viem";

export const accountAbi = parseAbi([
  "function entryPoint() view returns (address)",
  "function execute(address target, uint256 value, bytes data)",
]);
export const usdcTransferAbi = parseAbi([
  "function transfer(address to, uint256 amount) returns (bool)",
]);
export const nonceAbi = parseAbi([
  "function getNonce(address sender, uint192 key) view returns (uint256)",
]);
