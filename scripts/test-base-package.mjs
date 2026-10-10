#!/usr/bin/env node
import { reuseConsumerArchives } from "./consumer-archives.mjs";
import { buildConsumer } from "./build-consumer.mjs";
// Fresh optional-package consumer: no source aliases, WASM, SDK or account backend.
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import { createServer } from "node:http";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { tmpdir } from "node:os";
import { createExample } from "./create-example.mjs";
import { launchBrowser } from "./browser-launch.mjs";
const root = resolve(dirname(fileURLToPath(import.meta.url)), ".."),
  scratch = mkdtempSync(join(tmpdir(), "z-stack-base-consumer-"));
const app = join(scratch, "app"),
  archives = join(scratch, "archives");
function run(command, args, cwd = root) {
  if (command === "npm" && args.join(" ") === "run build") return buildConsumer(cwd, { env: { NODE_OPTIONS: "" } });
  const r = spawnSync(command, args, {
    cwd,
    stdio: "inherit",
    shell: process.platform === "win32",
    env: { ...process.env, NODE_OPTIONS: "" },
  });
  assert.equal(r.status, 0, `${command} failed`);
}
let server;
try {
  if (!reuseConsumerArchives(root, archives, ["base"]))
    run(process.execPath, ["scripts/pack-sdk.mjs", archives, "--base-only"]);
  createExample("base-wallet", app, archives);
  run("npm", ["install", "--ignore-scripts", "--no-audit", "--no-fund"], app);
  for (const pkg of ["sdk", "core", "passkey"])
    assert.ok(!existsSync(join(app, "node_modules/@z-stack", pkg)), `${pkg} must remain optional`);
  const manifest = JSON.parse(readFileSync(join(app, "node_modules/@z-stack/base/package.json")));
  assert.deepEqual(Object.keys(manifest.dependencies), ["viem"]);
  assert.ok(!existsSync(join(app, "node_modules/@z-stack/base/src")));
  const check = join(app, "consumer.mjs");
  writeFileSync(
    check,
    `import assert from 'node:assert/strict';
    import {BASE_SEPOLIA,createBaseWallet,deriveBaseAddress} from '@z-stack/base';
    import {createBaseSmartAccount,createBaseBundlerTransport,runBaseSponsoredTransfer} from '@z-stack/base/smart-account';
    const address=deriveBaseAddress('test test test test test test test test test test test junk');
    assert.equal(address,'0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266');
    assert.equal(createBaseWallet({address,network:BASE_SEPOLIA,rpcUrl:'http://localhost:8545'}).network.chain.id,84532);
    assert.equal(typeof createBaseSmartAccount,'function');assert.equal(typeof createBaseBundlerTransport,'function');assert.equal(typeof runBaseSponsoredTransfer,'function');`,
  );
  run(process.execPath, [check], app);
  run("npm", ["run", "build"], app);
  if (process.argv.includes("--browser")) {
    const { keccak256 } = createRequire(join(root, "packages/base/package.json"))("viem");
    let broadcasts = 0,
      acknowledgementLost = false;
    const transactions = new Map();
    server = createServer(async (req, res) => {
      if (req.url === "/rpc" && req.method === "POST") {
        let text = "";
        for await (const chunk of req) text += chunk;
        const rpc = JSON.parse(text);
        let result;
        const hash = "0x" + "ab".repeat(32),
          number = "0x64";
        switch (rpc.method) {
          case "eth_chainId":
            result = "0x14a34";
            break;
          case "eth_getBalance":
            result = "0x3635c9adc5dea00000";
            break;
          case "eth_getTransactionCount":
            result = "0x" + transactions.size.toString(16);
            break;
          case "eth_estimateGas":
            result = "0x5208";
            break;
          case "eth_maxPriorityFeePerGas":
          case "eth_gasPrice":
            result = "0x1";
            break;
          case "eth_getBlockByNumber":
            result = {
              number,
              hash,
              parentHash: "0x" + "cd".repeat(32),
              timestamp: "0x64",
              baseFeePerGas: "0xa",
              gasLimit: "0x1c9c380",
              gasUsed: "0x0",
              transactions: [],
            };
            break;
          case "eth_call":
            result =
              "0x" +
              (rpc.params[0].to.toLowerCase() === "0x036cbd53842c5426634e7929541ec2318f3dcf7e"
                ? "3b9aca00"
                : "1"
              ).padStart(64, "0");
            break;
          case "eth_sendRawTransaction": {
            broadcasts++;
            result = keccak256(rpc.params[0]);
            transactions.set(result, rpc.params[0]);
            if (acknowledgementLost) {
              res.writeHead(502, { "Content-Type": "application/json" });
              res.end(
                JSON.stringify({
                  error: "upstream acknowledgement lost after accepting the transaction",
                }),
              );
              return;
            }
            break;
          }
          case "eth_getTransactionReceipt":
            result = transactions.has(rpc.params[0])
              ? {
                  transactionHash: rpc.params[0],
                  transactionIndex: "0x0",
                  blockHash: hash,
                  blockNumber: number,
                  from: "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266",
                  to: "0x2222222222222222222222222222222222222222",
                  cumulativeGasUsed: "0x5208",
                  gasUsed: "0x5208",
                  effectiveGasPrice: "0xb",
                  logs: [],
                  logsBloom: "0x" + "00".repeat(256),
                  status: "0x1",
                  type: "0x2",
                }
              : null;
            break;
          default:
            res.writeHead(400);
            res.end("Unsupported fixture RPC " + rpc.method);
            return;
        }
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ jsonrpc: "2.0", id: rpc.id, result }));
        return;
      }
      const pathname = decodeURIComponent(new URL(req.url, "http://localhost").pathname),
        file = resolve(app, "dist", "." + (pathname === "/" ? "/index.html" : pathname));
      if (!file.startsWith(resolve(app, "dist") + "/") || !existsSync(file)) {
        res.writeHead(404);
        res.end();
        return;
      }
      res.writeHead(200, {
        "Content-Type": file.endsWith(".js")
          ? "text/javascript"
          : file.endsWith(".css")
            ? "text/css"
            : "text/html",
      });
      res.end(readFileSync(file));
    });
    await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
    const origin = `http://127.0.0.1:${server.address().port}`,
      playwright = createRequire(join(root, "package.json"))("playwright");
    const names = (
      process.argv.find((arg) => arg.startsWith("--browsers="))?.slice(11) ?? "chromium"
    ).split(",");
    for (const name of names) {
      assert.ok(["chromium", "firefox", "webkit"].includes(name));
      const browser = await launchBrowser(playwright[name]);
      try {
        const page = await browser.newPage({ viewport: { width: 390, height: 844 } });
        await page.goto(origin);
        await page.locator("#rpc").fill(origin + "/rpc");
        await page
          .locator("#secret")
          .fill("test test test test test test test test test test test junk");
        await page.getByRole("button", { name: "Open Base wallet" }).click();
        await page.waitForFunction(() =>
          document.getElementById("balance").textContent.includes("1000"),
        );
        assert.equal(await page.locator("#secret").inputValue(), "");
        assert.ok(await page.evaluate(() => document.body.scrollWidth <= innerWidth));
        await page.locator("#asset").selectOption("eth");
        await page.locator("#recipient").fill("0x2222222222222222222222222222222222222222");
        await page.locator("#amount").fill("0.01");
        await page.getByRole("button", { name: "Review payment" }).click();
        await page.locator("#approval").waitFor({ state: "visible" });
        const before = broadcasts;
        await page
          .locator("#unlock")
          .fill("test test test test test test test test test test test junk");
        await page.getByRole("button", { name: "Confirm and send" }).click();
        await page.waitForFunction(() =>
          document.getElementById("message").textContent.includes("Payment submitted"),
        );
        assert.equal(broadcasts, before + 1);
        assert.equal(await page.locator("#unlock").inputValue(), "");
        await page.getByRole("button", { name: "Review payment" }).click();
        await page.waitForFunction(() =>
          document.getElementById("message").textContent.includes("Check the saved payment"),
        );
        assert.equal(broadcasts, before + 1);
        await page.getByRole("button", { name: "Check saved payment" }).click();
        await page.waitForFunction(() =>
          document.getElementById("message").textContent.includes("Payment confirmed"),
        );
        // A real locally signed submission accepted by the fixture, with its HTTP acknowledgement lost.
        acknowledgementLost = true;
        await page.getByRole("button", { name: "Review payment" }).click();
        await page.locator("#approval").waitFor({ state: "visible" });
        await page
          .locator("#unlock")
          .fill("test test test test test test test test test test test junk");
        await page.getByRole("button", { name: "Confirm and send" }).click();
        await page.waitForFunction(() =>
          document.getElementById("message").textContent.includes("acknowledgement is unknown"),
        );
        assert.equal(broadcasts, before + 2);
        await page.reload();
        await page.locator("#rpc").fill(origin + "/rpc");
        await page
          .locator("#secret")
          .fill("test test test test test test test test test test test junk");
        await page.getByRole("button", { name: "Open Base wallet" }).click();
        await page
          .getByRole("button", { name: "Check saved payment" })
          .waitFor({ state: "visible" });
        await page.locator("#asset").selectOption("eth");
        await page.locator("#recipient").fill("0x2222222222222222222222222222222222222222");
        await page.locator("#amount").fill("0.01");
        await page.getByRole("button", { name: "Review payment" }).click();
        await page.waitForFunction(() =>
          document.getElementById("message").textContent.includes("Check the saved payment"),
        );
        assert.equal(broadcasts, before + 2);
        await page.getByRole("button", { name: "Check saved payment" }).click();
        await page.waitForFunction(() =>
          document.getElementById("message").textContent.includes("Payment confirmed"),
        );
        acknowledgementLost = false;
        console.log(
          `${name}: fresh archive, real local signature, durable lost-ACK recovery and 390px layout passed`,
        );
      } finally {
        await browser.close();
      }
    }
  }
  console.log("Optional Base archive builds and imports without SDK/core/passkey/WASM");
} finally {
  if (server) await new Promise((resolve) => server.close(resolve));
  rmSync(scratch, { recursive: true, force: true });
}
