import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer, type RequestListener } from "node:http";
import { once } from "node:events";
import { resolve } from "node:path";
import { test, type TestContext } from "node:test";
import { ROOT, assertLocalRegtestChain, ensureZWalletBuilt, observeServe, stop, waitHttp, waitServeReady, zWalletBin } from "../../../scripts/sdk-harness.mjs";

test("mining fixture identity requires a loopback validator and the full regtest activation schedule", () => {
  const upgrades = Object.fromEntries(Object.entries({ Overwinter: 1, Sapling: 1, Blossom: 1, Heartwood: 1, Canopy: 1,
    NU5: 2, NU6: 2, "NU6.1": 2, "NU6.2": 2, "NU6.3": 1_000_000 }).map(([name, activationheight]) => [name, { name, activationheight }]));
  const fixture = { chain: "test", upgrades };
  assert.doesNotThrow(() => assertLocalRegtestChain(fixture, "http://127.0.0.1:29232"));
  assert.doesNotThrow(() => assertLocalRegtestChain({ ...fixture, chain: "regtest" }, "http://[::1]:29232"));
  assert.throws(() => assertLocalRegtestChain({ chain: "test", upgrades: {} }, "http://127.0.0.1:29232"), /activation schedule/);
  assert.throws(() => assertLocalRegtestChain({ ...fixture, chain: "main" }, "http://127.0.0.1:29232"), /not a regtest/);
  assert.throws(() => assertLocalRegtestChain(fixture, "http://public.invalid:29232"), /loopback/);
  assert.throws(() => assertLocalRegtestChain({ ...fixture, upgrades: { ...upgrades,
    NU5: { name: "NU5", activationheight: 1_842_420 } } }, "http://127.0.0.1:29232"), /NU5 at 2/);
});

test("mining rejects an omitted future NU7 schedule and requires valid matching client heights", () => {
  const names = ["Z_STACK_REGTEST_NU6_3", "Z_STACK_REGTEST_NU7"];
  const before = names.map(name => process.env[name]);
  try {
    delete process.env.Z_STACK_REGTEST_NU6_3;
    delete process.env.Z_STACK_REGTEST_NU7;
    const upgrades = Object.fromEntries(Object.entries({ Overwinter: 1, Sapling: 1, Blossom: 1,
      Heartwood: 1, Canopy: 1, NU5: 2, NU6: 2, "NU6.1": 2, "NU6.2": 2, "NU6.3": 1_000_000,
      NU7: 1_000_001 }).map(([name, activationheight]) => [name, { name, activationheight }]));
    const fixture = { chain: "regtest", blocks: 1, upgrades };
    const rpc = "http://127.0.0.1:29232";
    assert.throws(() => assertLocalRegtestChain(fixture, rpc), /scheduled NU7/);
    assert.throws(() => assertLocalRegtestChain({ ...fixture, blocks: 1_000_001 }, rpc), /scheduled NU7/);
    process.env.Z_STACK_REGTEST_NU7 = "1000001";
    assert.doesNotThrow(() => assertLocalRegtestChain(fixture, rpc));
    process.env.Z_STACK_REGTEST_NU7 = "1000002";
    assert.throws(() => assertLocalRegtestChain(fixture, rpc), /activation schedule/);
    for (const invalid of ["0", "250", "1000000", "1000000.5", "4294967296", "not-a-height"]) {
      process.env.Z_STACK_REGTEST_NU7 = invalid;
      assert.throws(() => assertLocalRegtestChain(fixture, rpc), /uint32 after NU6.3/);
    }
  } finally {
    names.forEach((name, i) => {
      if (before[i] === undefined) delete process.env[name]; else process.env[name] = before[i];
    });
  }
});

async function server(t: TestContext, handler: RequestListener): Promise<string> {
  const http = createServer(handler).listen(0, "127.0.0.1");
  await once(http, "listening");
  t.after(async () => {
    http.closeAllConnections();
    await new Promise<void>((accept, reject) => http.close((error) => error ? reject(error) : accept()));
  });
  const address = http.address();
  assert.ok(address && typeof address === "object");
  return `http://127.0.0.1:${address.port}`;
}

function child(t: TestContext, script: string) {
  const proc = observeServe(spawn(process.execPath, ["-e", script], { stdio: ["ignore", "pipe", "pipe"] }));
  t.after(() => stop(proc, 100));
  return proc;
}

test("serve readiness captures a chunked token and proves authenticated ownership", async (t) => {
  let authorization: string | undefined;
  const url = await server(t, (req, res) => {
    if (req.url === "/health") { res.end('{"ok":true}'); return; }
    authorization = req.headers.authorization;
    res.statusCode = authorization === "Bearer fixture-token" ? 404 : 401;
    res.end("no wallet yet");
  });
  const proc = child(t, `process.stdout.write("listening\\ntok"); setTimeout(() => process.stdout.write("en fixture-token\\n"), 15); setInterval(() => {}, 1000);`);
  assert.deepEqual(await waitServeReady(proc, url, 2000), { token: "fixture-token" });
  assert.equal(authorization, "Bearer fixture-token");
});

test("open health from another process cannot pass serve authentication", async (t) => {
  const url = await server(t, (req, res) => {
    if (req.url !== "/health") res.statusCode = 401;
    res.end('{"ok":true}');
  });
  const proc = child(t, `console.log("token wrong-fixture-token"); setInterval(() => {}, 1000);`);
  await assert.rejects(waitServeReady(proc, url, 2000), /token does not match/);
});

test("serve readiness surfaces child exit and spawn failure without waiting for timeout", async (t) => {
  const proc = child(t, `process.stderr.write("fixture startup failed\\n"); process.exit(7);`);
  await assert.rejects(waitServeReady(proc, "http://127.0.0.1:1", 10_000), /exited \(7\).*fixture startup failed/);
  const missing = observeServe(spawn(resolve(ROOT, "fixture-binary-does-not-exist"), [], { stdio: "pipe" }));
  await assert.rejects(waitServeReady(missing, "http://127.0.0.1:1", 10_000), /ENOENT/);
  await stop(missing);
});

test("readiness timeout bounds a server that never sends headers or finishes JSON", async (t) => {
  for (const body of [false, true]) {
    const url = await server(t, (_req, res) => {
      if (body) { res.writeHead(200, { "content-type": "application/json" }); res.write('{"ok":'); }
    });
    const start = Date.now();
    await assert.rejects(waitHttp(url, 80), /timeout waiting/);
    assert.ok(Date.now() - start < 1500, "a stuck fetch or body must not outlive the readiness budget indefinitely");
  }
});

test("stop waits for an owned child and escalates an ignored SIGTERM", { skip: process.platform === "win32" }, async (t) => {
  const proc = child(t, `process.on("SIGTERM", () => {}); console.log("ready"); setInterval(() => {}, 1000);`);
  await once(proc.stdout!, "data");
  await stop(proc, 30);
  assert.equal(proc.signalCode, "SIGKILL");
  await stop(proc, 30);
});

test("binary selection respects explicit builds and Cargo target directories", async (t) => {
  const names = ["Z_STACK_SDK_WALLET_BIN", "Z_STACK_SDK_BUILD_PROFILE", "CARGO_TARGET_DIR"];
  const before = names.map((name) => process.env[name]);
  t.after(() => names.forEach((name, i) => {
    if (before[i] === undefined) delete process.env[name]; else process.env[name] = before[i];
  }));
  process.env.Z_STACK_SDK_WALLET_BIN = process.execPath;
  assert.equal(zWalletBin(), process.execPath);
  await ensureZWalletBuilt(); // Explicit existing binary: never invokes Cargo.
  delete process.env.Z_STACK_SDK_WALLET_BIN;
  process.env.CARGO_TARGET_DIR = "fixture-target";
  process.env.Z_STACK_SDK_BUILD_PROFILE = "debug";
  assert.equal(zWalletBin(), resolve(ROOT, "fixture-target/debug", process.platform === "win32" ? "z-wallet.exe" : "z-wallet"));
  process.env.Z_STACK_SDK_BUILD_PROFILE = "invalid";
  assert.throws(zWalletBin, /must be debug or release/);
});
