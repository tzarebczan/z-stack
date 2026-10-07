/**
 * Time a WASM compact-block catch-up against local Zaino via `z-wallet serve` /lwd.
 *
 *   node --import tsx scripts/wasm-sync-timing.mjs
 *
 * Env: Z_STACK_TIMING_SPAN (blocks, default 103680), Z_STACK_TIMING_BIND
 */
import { spawn } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  LOCAL_ZAINO_GRPC,
  LOCAL_ZAKURA_RPC_TESTNET,
  createWasmClient,
  httpLwdTransport,
  initialize,
  wasmCapabilities,
} from "../packages/sdk/src/lab.ts";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const SPAN = Number(process.env.Z_STACK_TIMING_SPAN || 103_680);
const BIND = process.env.Z_STACK_TIMING_BIND || "127.0.0.1:18790";
const WALLET = process.env.Z_STACK_TIMING_WALLET
  ? join(ROOT, process.env.Z_STACK_TIMING_WALLET)
  : join(ROOT, "target", "tmp-timing-90d");
const UFVK_JSON = process.env.Z_STACK_TIMING_UFVK_JSON || "";

function zWalletBin() {
  const name = process.platform === "win32" ? "z-wallet.exe" : "z-wallet";
  const rel = join(ROOT, "target", "release", name);
  const dbg = join(ROOT, "target", "debug", name);
  if (existsSync(rel)) return rel;
  if (existsSync(dbg)) return dbg;
  throw new Error("build z-wallet first (release preferred)");
}

function spawnServe() {
  const child = spawn(
    zWalletBin(),
    [
      "--wallet",
      WALLET,
      "--passphrase",
      "timing-bench",
      "--windows-credential",
      "false",
      "serve",
      "--bind",
      BIND,
    ],
    { cwd: ROOT, stdio: ["ignore", "pipe", "pipe"], windowsHide: true },
  );
  let buf = "";
  return new Promise((resolve, reject) => {
    const onData = (d) => {
      buf += d.toString();
      const m = buf.match(/token\s+(\S+)/);
      if (m) {
        child.stdout?.off("data", onData);
        child.stderr?.off("data", onData);
        resolve({ child, token: m[1] });
      }
    };
    child.stdout?.on("data", onData);
    child.stderr?.on("data", onData);
    child.on("error", reject);
    child.on("exit", (code) => {
      if (!buf.includes("token")) reject(new Error(`serve exited ${code}: ${buf.slice(-400)}`));
    });
    setTimeout(() => reject(new Error(`serve token timeout: ${buf.slice(-400)}`)), 20_000);
  });
}

const wasmPath = join(ROOT, "packages", "sdk", "src", "generated", "z_wasm_bg.wasm");
if (!existsSync(wasmPath)) throw new Error("run pnpm build:wasm first");

const { child, token } = await spawnServe();
const stop = () => {
  try {
    child.kill();
  } catch {
    /* ignore */
  }
};
process.on("exit", stop);
process.on("SIGINT", () => {
  stop();
  process.exit(1);
});

try {
  const rt = await initialize({
    wasmModule: readFileSync(wasmPath),
    prewarmProvingKey: false,
    prewarmProveWorker: false,
    preferMulticore: false,
  });
  const caps = wasmCapabilities();
  const transport = httpLwdTransport(
    `http://${BIND}`,
    "testnet",
    LOCAL_ZAINO_GRPC,
    LOCAL_ZAKURA_RPC_TESTNET,
    token,
  );
  const tip = await transport.tip();
  const birthday = Math.max(1, tip - SPAN);
  console.log(
    JSON.stringify({
      phase: "start",
      tip,
      birthday,
      span: tip - birthday + 1,
      simd: caps.simd,
      multicore: caps.multicore,
      runtime: rt,
      light: LOCAL_ZAINO_GRPC,
      restore: UFVK_JSON ? "ufvk" : "create",
    }),
  );

  let lastLog = 0;
  const client = createWasmClient(
    { network: "testnet", transport, allowDeepSync: false, autoShield: false },
    (p) => {
      const now = Date.now();
      if (now - lastLog < 5_000 && p.stage !== "synced") return;
      lastLog = now;
      console.log(
        JSON.stringify({
          phase: "progress",
          stage: p.stage,
          scanned: p.scanned,
          downloaded: p.downloaded,
          tip: p.tip,
          percent: p.percent,
          bps: p.blocksPerSecond,
          notes: p.notesFound,
          message: p.message,
        }),
      );
    },
  );
  if (UFVK_JSON) {
    const meta = JSON.parse(readFileSync(UFVK_JSON, "utf8"));
    if (!meta.ufvk) throw new Error(`${UFVK_JSON} has no ufvk`);
    await client.restoreUfvk(meta.ufvk, "testnet", birthday);
  } else {
    await client.create("testnet", birthday);
  }
  const t0 = Date.now();
  const after = await client.sync();
  const ms = Date.now() - t0;
  const blocks = Math.max(1, (after.scannedHeight ?? 0) - birthday + 1);
  console.log(
    JSON.stringify({
      phase: "done",
      ms,
      seconds: +(ms / 1000).toFixed(2),
      birthday,
      scanned: after.scannedHeight,
      tip,
      blocks,
      blkPerSec: +(blocks / (ms / 1000)).toFixed(1),
      spendReady: after.spendReady,
      treesReady: after.treesReady,
      notes: after.balance,
    }),
  );
} finally {
  stop();
}
