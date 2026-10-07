#!/usr/bin/env node
/**
 * Start `z-wallet pipe`: native gRPC to your Zaino, one HTTP/1.1 stream on loopback.
 *
 * Chrome will not speak cleartext HTTP/2, so the WASM lab needs this. Desktop
 * and `z-wallet serve` talk native gRPC and skip the pipe.
 *
 * Default endpoints (mainnet Zaino :8138). Bring-your-own node:
 *
 *   ZAINO_URL=http://127.0.0.1:8138
 *   Z_NETWORK=mainnet|testnet|regtest
 *   ZAKURA_RPC=http://127.0.0.1:8232   # or VALIDATOR_RPC / Z_STACK_VALIDATOR_RPC
 *   Z_PIPE_BIND=127.0.0.1:1239
 *
 *   pnpm lwd:pipe
 *   pnpm lwd:pipe -- --concurrency 8 --channels 2
 *
 * Extra argv after `--` are forwarded to `z-wallet pipe`.
 */
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const exe = process.platform === "win32" ? ".exe" : "";

const zaino =
  process.env.ZAINO_URL ||
  process.env.Z_STACK_PIPE_ZAINO ||
  "http://127.0.0.1:8138";
const network = process.env.Z_NETWORK || "mainnet";
const rpc =
  process.env.ZAKURA_RPC ||
  process.env.VALIDATOR_RPC ||
  process.env.Z_STACK_VALIDATOR_RPC ||
  process.env.Z_STACK_ZEBRA_RPC;
const bind = process.env.Z_PIPE_BIND || "127.0.0.1:1239";

const forwarded = process.argv.slice(2);
const pipeArgs = ["pipe", "--zaino", zaino, "--network", network, "--bind", bind];
if (rpc) {
  pipeArgs.push("--rpc", rpc);
}
pipeArgs.push(...forwarded);

function resolveCmd() {
  const release = join(root, "target", "release", `z-wallet${exe}`);
  const namedPipe = join(root, "target", "release", `z-wallet-pipe${exe}`);
  if (existsSync(release)) {
    return { cmd: release, args: pipeArgs, via: "release" };
  }
  if (existsSync(namedPipe)) {
    return { cmd: namedPipe, args: pipeArgs, via: "z-wallet-pipe" };
  }
  return {
    cmd: "cargo",
    args: [
      "run",
      "-p",
      "z-engine",
      "--release",
      "--features",
      "native,cli",
      "--bin",
      "z-wallet",
      "--",
      ...pipeArgs,
    ],
    via: "cargo",
  };
}

const { cmd, args, via } = resolveCmd();
console.log(`lwd-pipe via ${via}: ${cmd} ${args.join(" ")}`);
const child = spawn(cmd, args, {
  cwd: root,
  stdio: "inherit",
  windowsHide: false,
});
child.on("exit", (code, signal) => {
  if (signal) {
    process.exit(1);
  }
  process.exit(code ?? 1);
});
