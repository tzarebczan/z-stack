#!/usr/bin/env node
/**
 * Regtest on local Zakura + Zaino binaries (no Docker), with Ironwood.
 *
 *   node scripts/regtest-native.mjs up [--fresh]
 *   node scripts/regtest-native.mjs down
 *
 * Same ports as the compose stack (validator RPC :29232, Zaino gRPC :28137), so
 * the regtest tests and SDK scripts run unchanged. NU6.3 (Ironwood) activates
 * at Z_STACK_REGTEST_NU6_3 (default 150): blocks before it exercise Orchard,
 * blocks after it Ironwood. Export the same variable for z-wallet, cargo tests
 * and the SDK harness so the wallet uses the validator's schedule.
 *
 * Binaries: ZAKURAD / ZAINOD, else the newest `zakurad-*` / `zainod-*` under
 * ~/.local/share/z-stack/mainnet/bin. Chain data and logs live in
 * Z_STACK_REGTEST_DIR (default ~/.local/share/z-stack/regtest); `--fresh`
 * deletes that chain first.
 */
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, openSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { connect } from "node:net";
import { homedir } from "node:os";
import { join } from "node:path";
import { generate, waitForZebra, zebraRpc, ZEBRA_RPC } from "./regtest-rpc.mjs";

const cmd = process.argv[2];
const rpcPort = Number(new URL(ZEBRA_RPC).port || 80);
const lwdPort = Number(process.env.Z_STACK_REGTEST_LWD_PORT || 28137);
if (!Number.isInteger(lwdPort) || lwdPort < 1 || lwdPort > 65535) throw new Error("Invalid regtest light-server port");
const fresh = process.argv.includes("--fresh");
const dir = process.env.Z_STACK_REGTEST_DIR || join(homedir(), ".local", "share", "z-stack", "regtest");
const nu63 = Number(process.env.Z_STACK_REGTEST_NU6_3 || 150);
const nu7 = process.env.Z_STACK_REGTEST_NU7 === undefined ? undefined : Number(process.env.Z_STACK_REGTEST_NU7);
const binDir = join(homedir(), ".local", "share", "z-stack", "mainnet", "bin");
const FAUCET_TADDR = "tmV1zYhR2xisn6VWdCNKHpeD4S7L1U1nPH6"; // keys::REGTEST_FAUCET_TRANSPARENT

function newest(prefix) {
  if (!existsSync(binDir)) return undefined;
  const names = readdirSync(binDir).filter((n) => n.startsWith(`${prefix}-`)).sort();
  return names.length ? join(binDir, names[names.length - 1]) : undefined;
}

function pidFile(name) {
  return join(dir, `${name}.pid`);
}

function running(name) {
  if (!existsSync(pidFile(name))) return undefined;
  const pid = Number(readFileSync(pidFile(name), "utf8"));
  try {
    process.kill(pid, 0);
    return pid;
  } catch {
    return undefined;
  }
}

function stop(name) {
  const pid = running(name);
  if (pid) process.kill(pid, "SIGTERM");
  rmSync(pidFile(name), { force: true });
  return pid;
}

function start(name, bin, args) {
  const log = openSync(join(dir, "logs", `${name}.log`), "a");
  const child = spawn(bin, args, { cwd: dir, detached: true, stdio: ["ignore", log, log] });
  child.unref();
  writeFileSync(pidFile(name), String(child.pid));
}

async function waitPort(port, timeoutMs = 60_000) {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const open = await new Promise((resolve) => {
      const s = connect(port, "127.0.0.1", () => {
        s.end();
        resolve(true);
      });
      s.on("error", () => resolve(false));
    });
    if (open) return;
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`nothing listening on 127.0.0.1:${port}; see ${join(dir, "logs")}`);
}

function zakuraConfig() {
  return `# Written by scripts/regtest-native.mjs.
[network]
network = "Regtest"
listen_addr = "127.0.0.1:28233"
p2p_stack = "legacy"
initial_testnet_peers = []
cache_dir = "${join(dir, "peer")}"
identity_dir = "${join(dir, "identity")}"

[network.testnet_parameters.activation_heights]
BeforeOverwinter = 1
Overwinter = 1
Sapling = 1
Blossom = 1
Heartwood = 1
Canopy = 1
NU5 = 2
NU6 = 2
"NU6.1" = 2
"NU6.2" = 2
"NU6.3" = ${nu63}
${nu7 === undefined ? "" : `NU7 = ${nu7}`}

# Zakura requires a lockbox disbursement in the NU6.1 activation block. The
# regtest lockbox is empty (NU6 and NU6.1 share height 2), so disburse zero.
[[network.testnet_parameters.lockbox_disbursements]]
address = "t2RnBRiqrN1nW4ecZs1Fj3WWjNdnSs4kiX8"
amount = 0

[state]
cache_dir = "${join(dir, "zakura")}"

[rpc]
listen_addr = "127.0.0.1:${rpcPort}"
enable_cookie_auth = false

[mining]
miner_address = "${FAUCET_TADDR}"

[health]
listen_addr = "127.0.0.1:28080"
min_connected_peers = 0

[tracing]
use_color = false
`;
}

function zainoConfig() {
  // Zaino adopts the activation heights from the validator.
  return `# Written by scripts/regtest-native.mjs.
backend = "rpc"
network = "Regtest"
ephemeral_finalised_state = false
zebra_db_path = "${join(dir, "zakura")}"

[grpc_settings]
listen_address = "127.0.0.1:${lwdPort}"

[validator_settings]
validator_jsonrpc_listen_address = "127.0.0.1:${rpcPort}"
validator_user = "zebra"
validator_password = "zebra"

[storage.database]
path = "${join(dir, "zaino")}"
`;
}

if (cmd === "down") {
  for (const name of ["zaino", "zakura"]) {
    const pid = stop(name);
    console.log(pid ? `stopped ${name} (${pid})` : `${name} not running`);
  }
  process.exit(0);
}

if (cmd !== "up") {
  console.error("usage: regtest-native.mjs up [--fresh] | down");
  process.exit(2);
}

const zakurad = process.env.ZAKURAD || newest("zakurad");
const zainod = process.env.ZAINOD || newest("zainod");
if (!zakurad || !zainod) {
  console.error(`set ZAKURAD and ZAINOD (nothing under ${binDir})`);
  process.exit(1);
}
if (!Number.isInteger(nu63) || nu63 < 3) {
  console.error("Z_STACK_REGTEST_NU6_3 must be an integer >= 3");
  process.exit(1);
}
if (nu7 !== undefined && (!Number.isInteger(nu7) || nu7 <= nu63 || nu7 > 0xffff_ffff)) {
  throw new Error("Z_STACK_REGTEST_NU7 must be an integer after NU6.3 and within uint32");
}
if (fresh) {
  stop("zaino");
  stop("zakura");
  await new Promise((r) => setTimeout(r, 1000));
  for (const d of ["zakura", "zaino", "peer"]) rmSync(join(dir, d), { recursive: true, force: true });
  rmSync(join(dir, "nu63"), { force: true });
  rmSync(join(dir, "nu7"), { force: true });
}
const recorded = existsSync(join(dir, "nu63")) ? Number(readFileSync(join(dir, "nu63"), "utf8")) : undefined;
if (recorded !== undefined && recorded !== nu63) {
  console.error(`this chain has NU6.3 at ${recorded}; use Z_STACK_REGTEST_NU6_3=${recorded} or --fresh`);
  process.exit(1);
}
const recordedNu7 = existsSync(join(dir, "nu7")) ? readFileSync(join(dir, "nu7"), "utf8") : "unscheduled";
if (recorded !== undefined && recordedNu7 !== String(nu7 ?? "unscheduled")) {
  throw new Error("this chain has a different NU7 schedule; restore its setting or use --fresh");
}
for (const d of ["zakura", "zaino", "peer", "logs"]) mkdirSync(join(dir, d), { recursive: true });
mkdirSync(join(dir, "identity"), { recursive: true, mode: 0o700 });
writeFileSync(join(dir, "zakura.toml"), zakuraConfig());
writeFileSync(join(dir, "zaino.toml"), zainoConfig());
writeFileSync(join(dir, "nu63"), String(nu63));
writeFileSync(join(dir, "nu7"), String(nu7 ?? "unscheduled"));

if (!running("zakura")) start("zakura", zakurad, ["-c", join(dir, "zakura.toml"), "start"]);
let info = await waitForZebra();
// New validators can expose RPC just before genesis reaches durable state.
const genesisDeadline = Date.now() + 30_000;
while (!info.bestblockhash || /^0+$/.test(info.bestblockhash)) {
  if (Date.now() >= genesisDeadline) throw new Error("validator genesis not ready");
  await new Promise(resolve => setTimeout(resolve, 250));
  info = await zebraRpc("getblockchaininfo");
}
const scheduled = Object.values(info.upgrades ?? {}).find((u) => u.name === "NU6.3")?.activationheight;
if (scheduled !== nu63) {
  console.error(`validator reports NU6.3 at ${scheduled}, expected ${nu63}; run down, then up --fresh`);
  process.exit(1);
}
const scheduledNu7 = Object.values(info.upgrades ?? {}).find((u) => u.name === "NU7")?.activationheight;
if (scheduledNu7 !== nu7) {
  throw new Error(`validator reports NU7 at ${scheduledNu7}, expected ${nu7 ?? "unscheduled"}; match the client and validator schedules`);
}
if ((Number(info.blocks) || 0) < 2) await generate(2 - (Number(info.blocks) || 0));
if (!running("zaino")) start("zaino", zainod, ["start", "--config", join(dir, "zaino.toml")]);
await waitPort(lwdPort);
const after = await waitForZebra();
console.log(`ready: height=${after.blocks}, NU6.3 at ${nu63}, NU7 ${nu7 ?? "unscheduled"}`);
console.log(`  validator RPC http://127.0.0.1:${rpcPort}   Zaino gRPC http://127.0.0.1:${lwdPort}`);
console.log(`  export Z_STACK_REGTEST_NU6_3=${nu63}${nu7 === undefined ? "" : ` Z_STACK_REGTEST_NU7=${nu7}`}   logs: ${join(dir, "logs")}`);
