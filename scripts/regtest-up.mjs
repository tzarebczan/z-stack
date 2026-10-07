#!/usr/bin/env node
/**
 * Start Zebra + Zaino regtest, wait until RPC is up, mine initial activation blocks (height 2; NU6.3 defaults to 1,000,000).
 */
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { generate, waitForZebra, zebraRpc } from "./regtest-rpc.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const compose = join(root, "infra", "compose", "docker-compose.regtest.yml");

function docker(args) {
  const r = spawnSync("docker", args, { cwd: root, stdio: "inherit", shell: process.platform === "win32" });
  if (r.status !== 0) {
    process.exit(r.status ?? 1);
  }
}

docker(["compose", "-f", compose, "up", "-d"]);
console.log("waiting for Zebra RPC…");
const info = await waitForZebra();
console.log(`zebra chain=${info.chain} blocks=${info.blocks}`);
const height = Number(info.blocks) || 0;
if (height < 2) {
  console.log(`mining ${2 - height} activation block(s)`);
  await generate(2 - height);
}
const after = await zebraRpc("getblockchaininfo");
console.log(`ready: height=${after.blocks} (Zaino gRPC http://127.0.0.1:${process.env.Z_STACK_REGTEST_LWD_PORT ?? "28137"})`);
