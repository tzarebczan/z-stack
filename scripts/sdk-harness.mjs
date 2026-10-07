/** Spawn loopback `z-wallet serve` for SDK integration tests. */
import { spawn } from "node:child_process";
import { existsSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

/** Guard a mining fixture before generate: Zebra calls regtest "test" too. */
export function assertLocalRegtestChain(info, rpcUrl) {
  const url = new URL(rpcUrl);
  if (!["127.0.0.1", "localhost", "[::1]"].includes(url.hostname) || url.protocol !== "http:") {
    throw new Error("regtest fixture requires an explicit loopback HTTP validator");
  }
  if (info?.chain !== "test" && info?.chain !== "regtest") throw new Error("validator is not a regtest fixture");
  // Keep in lockstep with infra/compose/regtest/zebra.toml and Network::Regtest.
  const expected = { Overwinter: 1, Sapling: 1, Blossom: 1, Heartwood: 1, Canopy: 1,
    NU5: 2, NU6: 2, "NU6.1": 2, "NU6.2": 2,
    "NU6.3": Number(process.env.Z_STACK_REGTEST_NU6_3 || 1_000_000) };
  const upgrades = Object.values(info.upgrades ?? {});
  for (const [name, height] of Object.entries(expected)) {
    if (!upgrades.some((upgrade) => upgrade.name === name && upgrade.activationheight === height)) {
      throw new Error(`validator does not match regtest activation schedule: ${name} at ${height}`);
    }
  }
}

const observed = new WeakMap();

function buildProfile() {
  const profile = process.env.Z_STACK_SDK_BUILD_PROFILE ?? "release";
  if (profile !== "debug" && profile !== "release") throw new Error("Z_STACK_SDK_BUILD_PROFILE must be debug or release");
  return profile;
}

export function zWalletBin() {
  if (process.env.Z_STACK_SDK_WALLET_BIN) return resolve(ROOT, process.env.Z_STACK_SDK_WALLET_BIN);
  const name = process.platform === "win32" ? "z-wallet.exe" : "z-wallet";
  return resolve(ROOT, process.env.CARGO_TARGET_DIR ?? "target", buildProfile(), name);
}

export async function ensureZWalletBuilt() {
  // Explicit binary selection is useful for an already-built release fixture.
  if (!process.env.Z_STACK_SDK_WALLET_BIN) {
    await new Promise((accept, reject) => {
      const args = ["build", "-p", "z-engine", "--features", "native,cli", "--bin", "z-wallet"];
      if (buildProfile() === "release") args.push("--release");
      const child = spawn("cargo", args, { cwd: ROOT, stdio: "inherit", windowsHide: true });
      child.once("error", reject);
      child.once("exit", (code, signal) => code === 0 ? accept() : reject(new Error(`cargo build z-wallet failed (${code ?? signal})`)));
    });
  }
  if (!existsSync(zWalletBin())) throw new Error(`missing ${zWalletBin()}`);
}

/** Observe a child immediately so startup errors cannot become unhandled events. */
export function observeServe(child) {
  if (observed.has(child)) return child;
  const state = { token: undefined, failure: undefined, stderr: "", stdout: "", tokenWaiters: new Set() };
  observed.set(child, state);
  const wake = () => { for (const notify of state.tokenWaiters) notify(); };
  child.once("error", (error) => { state.failure = error; wake(); });
  child.once("exit", (code, signal) => {
    state.failure ??= new Error(`z-wallet serve exited (${code ?? signal})${state.stderr ? `: ${state.stderr.trim()}` : ""}`);
    wake();
  });
  child.stdout?.on("data", (chunk) => {
    state.stdout += chunk.toString();
    for (;;) {
      const end = state.stdout.indexOf("\n");
      if (end < 0) break;
      const line = state.stdout.slice(0, end).trim();
      state.stdout = state.stdout.slice(end + 1);
      const match = /^token\s+(\S+)$/.exec(line);
      if (match) { state.token = match[1]; wake(); }
    }
    state.stdout = state.stdout.slice(-8192);
    // Always drain stdout, but never log the authentication token.
  });
  child.stderr?.on("data", (chunk) => {
    const text = chunk.toString();
    state.stderr = (state.stderr + text).slice(-8192);
    if (process.env.Z_STACK_SDK_TEST_LOG) process.stderr.write(text);
  });
  return child;
}

export function spawnServe({ walletDir, bind, passphrase = "regtest" }) {
  mkdirSync(walletDir, { recursive: true });
  const child = spawn(
    zWalletBin(),
    [
      "--wallet",
      walletDir,
      "--windows-credential",
      "false",
      "serve",
      "--bind",
      bind,
    ],
    { cwd: ROOT, stdio: ["ignore", "pipe", "pipe"], windowsHide: true,
      env: { ...process.env, Z_STACK_PASSPHRASE: passphrase } },
  );
  return observeServe(child);
}

export async function waitHttp(url, timeoutMs = 30_000, { child } = {}) {
  const start = Date.now();
  let last = "";
  while (Date.now() - start < timeoutMs) {
    if (child && observed.get(child)?.failure) throw observed.get(child).failure;
    try {
      const remaining = Math.max(1, timeoutMs - (Date.now() - start));
      const res = await fetch(url, { signal: AbortSignal.timeout(Math.min(1000, remaining)) });
      if (res.ok) return await res.json();
      last = `HTTP ${res.status}`;
      await res.body?.cancel();
    } catch (e) {
      last = e instanceof Error ? e.message : String(e);
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`timeout waiting for ${url}: ${last}`);
}

export async function waitServeReady(child, baseUrl, timeoutMs = 30_000) {
  const state = observed.get(child);
  if (!state) throw new Error("serve child is not observed");
  const started = Date.now();
  const token = await new Promise((accept, reject) => {
    const finish = () => {
      if (!state.failure && !state.token) return;
      clearTimeout(timer);
      state.tokenWaiters.delete(finish);
      if (state.failure) reject(state.failure); else accept(state.token);
    };
    const timer = setTimeout(() => {
      state.tokenWaiters.delete(finish);
      reject(new Error("z-wallet serve did not publish an authentication token"));
    }, timeoutMs);
    state.tokenWaiters.add(finish);
    finish();
  });
  const remaining = () => Math.max(1, timeoutMs - (Date.now() - started));
  await waitHttp(`${baseUrl}/health`, remaining(), { child });
  // /health is open. Verify the token too, so an unrelated process already on
  // this port cannot pass readiness and receive subsequent wallet mutations.
  const response = await fetch(`${baseUrl}/wallet`, {
    headers: { authorization: `Bearer ${token}` }, signal: AbortSignal.timeout(remaining()),
  });
  await response.arrayBuffer();
  if (state.failure) throw state.failure;
  if (response.status === 401 || response.status === 403) throw new Error("serve token does not match the process listening on this port");
  return { token };
}

export async function stop(child, timeoutMs = 5000) {
  if (!child || !child.pid || child.exitCode !== null || child.signalCode !== null) return;
  await new Promise((accept, reject) => {
    let force;
    let expiry;
    const cleanup = () => { clearTimeout(force); clearTimeout(expiry); child.removeListener("exit", onExit); };
    const onExit = () => { cleanup(); accept(); };
    child.once("exit", onExit);
    force = setTimeout(() => { child.kill("SIGKILL"); }, timeoutMs);
    expiry = setTimeout(() => { cleanup(); reject(new Error("z-wallet serve did not exit after termination")); }, timeoutMs + 2000);
    if (!child.killed) child.kill();
  });
}
