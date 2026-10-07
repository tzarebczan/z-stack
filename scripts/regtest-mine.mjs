#!/usr/bin/env node
import { generate, waitForZebra, zebraRpc } from "./regtest-rpc.mjs";

const n = Number(process.argv[2] ?? "1");
await waitForZebra();
const hashes = await generate(n);
const info = await zebraRpc("getblockchaininfo");
console.log(`mined ${Array.isArray(hashes) ? hashes.length : n}; height=${info.blocks}`);
