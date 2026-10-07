import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { readFileSync, existsSync } from "node:fs";
import { join, resolve } from "node:path";
const root = resolve(import.meta.dirname, "..");
const require = createRequire(join(root, "packages/base/package.json"));
const {
  createPublicClient,
  http,
  encodeFunctionData,
  parseEther,
  keccak256,
  parseTransaction,
} = require("viem");
const { mnemonicToAccount } = require("viem/accounts");
const USDC = "0x036CbD53842c5426634e7929541eC2318f3dCF7e",
  ORACLE = "0x420000000000000000000000000000000000000F";
const tokenAbi = [
  {
    type: "function",
    name: "mint",
    stateMutability: "nonpayable",
    inputs: [{ type: "address" }, { type: "uint256" }],
    outputs: [],
  },
  {
    type: "function",
    name: "balanceOf",
    stateMutability: "view",
    inputs: [{ type: "address" }],
    outputs: [{ type: "uint256" }],
  },
];
/** Real isolated EVM, plus a CORS proxy which can discard one accepted submission's ACK. */
export async function baseDemoFixture(origins, words, { chainId = 84532 } = {}) {
  assert.ok([84532, 8453].includes(chainId));
  const token = chainId === 8453 ? "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913" : USDC;
  const tools = process.env.Z_STACK_BASE_FIXTURE_TOOLS;
  assert.ok(
    tools,
    "Set Z_STACK_BASE_FIXTURE_TOOLS to an npm prefix containing Anvil and solc; see docs/BASE-WALLET.md",
  );
  const binary = join(tools, "node_modules/@foundry-rs/anvil-linux-amd64/bin/anvil");
  assert.ok(existsSync(binary), "This local fixture currently needs the Linux x64 Anvil package");
  const solc = createRequire(join(resolve(tools), "package.json"))("solc");
  const compiled = JSON.parse(
    solc.compile(
      JSON.stringify({
        language: "Solidity",
        sources: {
          "fixture.sol": {
            content: readFileSync(join(root, "scripts/fixtures/base-demo.sol"), "utf8"),
          },
        },
        settings: { outputSelection: { "*": { "*": ["evm.deployedBytecode.object"] } } },
      }),
    ),
  );
  assert.ok(
    !compiled.errors?.some((error) => error.severity === "error"),
    JSON.stringify(compiled.errors),
  );
  const reservation = createServer();
  await new Promise((resolve) => reservation.listen(0, "127.0.0.1", resolve));
  const port = reservation.address().port;
  await new Promise((resolve) => reservation.close(resolve));
  const node = spawn(
    binary,
    ["--host", "127.0.0.1", "--port", String(port), "--chain-id", String(chainId), "--silent"],
    { stdio: "ignore" },
  );
  const direct = `http://127.0.0.1:${port}`;
  async function rpc(method, params = []) {
    const response = await fetch(direct, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
    });
    const row = await response.json();
    assert.ok(!row.error, JSON.stringify(row.error));
    return row.result;
  }
  let proxy;
  try {
    let ready = false;
    for (let attempt = 0; attempt < 100; attempt++) {
      assert.equal(node.exitCode, null, "Owned Anvil exited before readiness");
      try {
        if ((await rpc("eth_chainId")) === "0x" + chainId.toString(16)) {
          ready = true;
          break;
        }
      } catch {}
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    assert.ok(ready, "Anvil did not become ready");
    for (const [address, name] of [
      [token, "DemoUsdc"],
      [ORACLE, "DemoOracle"],
    ])
      await rpc("anvil_setCode", [
        address,
        "0x" + compiled.contracts["fixture.sol"][name].evm.deployedBytecode.object,
      ]);
    const owner = mnemonicToAccount(words).address;
    await rpc("anvil_setBalance", [owner, "0x" + parseEther("100").toString(16)]);
    await rpc("eth_sendTransaction", [
      {
        from: (await rpc("eth_accounts"))[0],
        to: token,
        data: encodeFunctionData({
          abi: tokenAbi,
          functionName: "mint",
          args: [owner, 100_000_000n],
        }),
      },
    ]);
    let lost = false,
      rejected = false,
      wrongChain = false,
      broadcasts = 0;
    const hashes = [],
      signedAttempts = [];
    proxy = createServer(async (req, res) => {
      const headers = {
        "access-control-allow-origin": origins.includes(req.headers.origin)
          ? req.headers.origin
          : "null",
        vary: "Origin",
        "access-control-allow-methods": "POST, OPTIONS",
        "access-control-allow-headers": "content-type",
        "content-type": "application/json",
        "cross-origin-resource-policy": "cross-origin",
        "cache-control": "no-store",
      };
      if (req.method === "OPTIONS") {
        res.writeHead(204, headers).end();
        return;
      }
      if (req.method !== "POST" || !origins.includes(req.headers.origin)) {
        res.writeHead(403, headers).end();
        return;
      }
      try {
        let body = "";
        for await (const chunk of req) {
          body += chunk;
          assert.ok(body.length < 100_000);
        }
        const request = JSON.parse(body);
        assert.ok(!Array.isArray(request), "No RPC batches in this fixture");
        // A public browser never gets access to Anvil's privileged test controls.
        assert.ok(
          /^(eth_|net_|web3_)/.test(request.method) && request.method !== "eth_sendTransaction",
          "Disallowed browser method",
        );
        if (request.method === "eth_sendRawTransaction") {
          signedAttempts.push(request.params[0]);
          if (rejected) {
            rejected = false;
            res.writeHead(200, headers).end(
              JSON.stringify({
                jsonrpc: "2.0",
                id: request.id,
                error: { code: -32000, message: "Fixture rejected before forwarding" },
              }),
            );
            return;
          }
        }
        let result =
          wrongChain && request.method === "eth_chainId"
            ? "0x1"
            : await rpc(request.method, request.params);
        if (request.method === "eth_sendRawTransaction") {
          broadcasts++;
          const hash = keccak256(request.params[0]);
          assert.equal(result.toLowerCase(), hash.toLowerCase());
          hashes.push(hash);
          if (lost) {
            lost = false;
            res
              .writeHead(502, headers)
              .end(JSON.stringify({ error: "Acknowledgement lost after actual EVM acceptance" }));
            return;
          }
        }
        res.writeHead(200, headers).end(JSON.stringify({ jsonrpc: "2.0", id: request.id, result }));
      } catch {
        res.writeHead(500, headers).end(JSON.stringify({ error: "Local EVM fixture failed" }));
      }
    });
    await new Promise((resolve) => proxy.listen(0, "127.0.0.1", resolve));
    const client = createPublicClient({ transport: http(direct, { retryCount: 0 }) });
    return {
      url: `http://127.0.0.1:${proxy.address().port}`,
      owner,
      hashes,
      signedAttempts,
      rejectNextSubmission() {
        rejected = true;
      },
      async consumeLastRejectedNonce() {
        const parsed = parseTransaction(signedAttempts.at(-1));
        const signed = await mnemonicToAccount(words).signTransaction({
          chainId,
          type: "eip1559",
          to: owner,
          value: 0n,
          nonce: parsed.nonce,
          gas: 21000n,
          maxFeePerGas: parsed.maxFeePerGas,
          maxPriorityFeePerGas: parsed.maxPriorityFeePerGas,
        });
        return rpc("eth_sendRawTransaction", [signed]);
      },
      get broadcasts() {
        return broadcasts;
      },
      loseNextAcknowledgement() {
        lost = true;
      },
      wrongChain(value) {
        wrongChain = value;
      },
      mine: () => rpc("anvil_mine", ["0x50"]),
      eth: (address) => client.getBalance({ address }),
      usdc: (address) =>
        client.readContract({
          address: token,
          abi: tokenAbi,
          functionName: "balanceOf",
          args: [address],
        }),
      receipt: (hash) => client.getTransactionReceipt({ hash }),
      async stop() {
        proxy.closeAllConnections();
        await new Promise((resolve) => proxy.close(resolve));
        node.kill("SIGTERM");
        await new Promise((resolve) => node.once("exit", resolve));
      },
    };
  } catch (error) {
    proxy?.closeAllConnections();
    proxy?.close();
    node.kill("SIGTERM");
    throw error;
  }
}

export async function exerciseCombinedBase(page, fixture, words) {
  const recipient = "0x2222222222222222222222222222222222222222";
  await page.getByLabel("Base Sepolia RPC", { exact: true }).fill(fixture.url);
  await page
    .getByLabel("Your Zcash recovery phrase", { exact: true })
    .fill("abandon ".repeat(23) + "art");
  const before = fixture.broadcasts;
  await page.getByRole("button", { name: "Enable Base Sepolia", exact: true }).click();
  await page.waitForFunction(() =>
    document.getElementById("base-message").textContent.includes("match"),
  );
  assert.equal(await page.locator("#base-secret").inputValue(), "");
  assert.equal(fixture.broadcasts, before);
  await page.getByLabel("Your Zcash recovery phrase", { exact: true }).fill(words);
  await page.getByRole("button", { name: "Enable Base Sepolia", exact: true }).click();
  await page.waitForFunction(() =>
    document.getElementById("base-balance").textContent.includes("USDC"),
  );
  assert.equal(await page.locator("#base-address").innerText(), fixture.owner);
  async function draft(asset, amount) {
    await page.locator("#base-asset").selectOption(asset);
    await page.getByLabel("Base recipient", { exact: true }).fill(recipient);
    await page.getByLabel("Base amount", { exact: true }).fill(amount);
    await page.getByRole("button", { name: "Review Base payment", exact: true }).click();
    await page.locator("#base-approval").waitFor({ state: "visible" });
  }
  async function send(unknown = false) {
    await page.getByLabel("Zcash recovery phrase for Base payment", { exact: true }).fill(words);
    if (unknown) fixture.loseNextAcknowledgement();
    await page.getByRole("button", { name: "Send Base payment", exact: true }).click();
    assert.equal(
      await page.locator("#base-unlock").inputValue(),
      "",
      "Phrase clears before asynchronous unlock",
    );
    await page.waitForFunction(
      unknown
        ? () =>
            document
              .getElementById("base-message")
              .textContent.includes("acknowledgement is unknown")
        : () =>
            document.getElementById("base-message").textContent.includes("Base payment submitted"),
      null,
      { timeout: 60_000 },
    );
  }
  async function confirm() {
    await fixture.mine();
    await page.getByRole("button", { name: "Check saved Base payment", exact: true }).click();
    await page.waitForFunction(
      () => document.getElementById("base-message").textContent === "Base payment confirmed.",
    );
  }
  // A wrong-chain RPC cannot produce a review or signature.
  fixture.wrongChain(true);
  await page.locator("#base-asset").selectOption("eth");
  await page.getByLabel("Base recipient", { exact: true }).fill(recipient);
  await page.getByLabel("Base amount", { exact: true }).fill("0.001");
  await page.getByRole("button", { name: "Review Base payment", exact: true }).click();
  await page.waitForFunction(() =>
    document.getElementById("base-message").textContent.includes("network could not be verified"),
  );
  assert.equal(fixture.broadcasts, before);
  fixture.wrongChain(false);
  // Fresh approval must identify a wrong phrase as an unlock error, without signing.
  await draft("usdc", "0.5");
  await page
    .getByLabel("Zcash recovery phrase for Base payment", { exact: true })
    .fill("abandon ".repeat(23) + "art");
  await page.getByRole("button", { name: "Send Base payment", exact: true }).click();
  await page.waitForFunction(() =>
    document.getElementById("base-message").textContent.includes("match"),
  );
  assert.equal(fixture.broadcasts, before);
  assert.equal(await page.locator("#base-unlock").inputValue(), "");
  assert.equal(
    await page
      .locator("#base-message")
      .innerText()
      .then((text) => text.includes("network is unavailable")),
    false,
  );
  const initialEth = await fixture.eth(recipient),
    initialUsdc = await fixture.usdc(recipient);
  await draft("eth", "0.001");
  await send();
  assert.equal(fixture.broadcasts, before + 1);
  assert.equal((await fixture.eth(recipient)) - initialEth, 10n ** 15n);
  await confirm();
  await draft("usdc", "1.25");
  await send();
  assert.equal(fixture.broadcasts, before + 2);
  assert.equal((await fixture.usdc(recipient)) - initialUsdc, 1_250_000n);
  const receipt = await fixture.receipt(fixture.hashes.at(-1));
  assert.equal(receipt.status, "success");
  assert.equal(receipt.logs.length, 1);
  await confirm();
  await draft("eth", "0.001");
  await send(true);
  assert.equal(fixture.broadcasts, before + 3);
  await page.reload();
  await page.getByLabel("Base Sepolia RPC", { exact: true }).fill(fixture.url);
  await page.getByLabel("Your Zcash recovery phrase", { exact: true }).fill(words);
  await page.getByRole("button", { name: "Enable Base Sepolia", exact: true }).click();
  await page
    .getByRole("button", { name: "Check saved Base payment", exact: true })
    .waitFor({ state: "visible" });
  await page.locator("#base-asset").selectOption("eth");
  await page.getByLabel("Base recipient", { exact: true }).fill(recipient);
  await page.getByLabel("Base amount", { exact: true }).fill("0.001");
  await page.getByRole("button", { name: "Review Base payment", exact: true }).click();
  await page.waitForFunction(() =>
    document.getElementById("base-message").textContent.includes("Check the saved payment"),
  );
  assert.equal(fixture.broadcasts, before + 3);
  await confirm();
  // A definite fixture refusal is indistinguishable from a lost network ACK to
  // the app. Keep its reservation, then explicitly retry identical signed bytes.
  const beforeRetryBalance = await fixture.eth(recipient);
  await draft("eth", "0.001");
  fixture.rejectNextSubmission();
  await page.getByLabel("Zcash recovery phrase for Base payment", { exact: true }).fill(words);
  await page.getByRole("button", { name: "Send Base payment", exact: true }).click();
  await page.waitForFunction(() =>
    document.getElementById("base-message").textContent.includes("acknowledgement is unknown"),
  );

  assert.equal(await fixture.eth(recipient), beforeRetryBalance);
  const refusedBytes = fixture.signedAttempts.at(-1);
  await page.getByRole("button", { name: "Check saved Base payment", exact: true }).click();
  await page
    .getByRole("button", { name: "Retry saved transaction", exact: true })
    .waitFor({ state: "visible" });
  page.once("dialog", (dialog) => dialog.accept());
  await page.getByRole("button", { name: "Retry saved transaction", exact: true }).click();
  await page.waitForFunction(() =>
    document.getElementById("base-message").textContent.includes("Saved transaction submitted"),
  );
  assert.equal(fixture.signedAttempts.at(-1), refusedBytes);
  assert.equal((await fixture.eth(recipient)) - beforeRetryBalance, 10n ** 15n);
  await confirm();
  // A replaced nonce cannot execute again, but its original payment outcome
  // stays unverified until the user checks chain activity and explicitly archives.
  await draft("eth", "0.001");
  fixture.rejectNextSubmission();
  await page.getByLabel("Zcash recovery phrase for Base payment", { exact: true }).fill(words);
  await page.getByRole("button", { name: "Send Base payment", exact: true }).click();
  await page.waitForFunction(() =>
    document.getElementById("base-message").textContent.includes("acknowledgement is unknown"),
  );
  await fixture.consumeLastRejectedNonce();
  await fixture.mine();
  await page.getByRole("button", { name: "Check saved Base payment", exact: true }).click();
  await page.locator("#base-archive").waitFor({ state: "visible" });
  assert.equal(await page.locator("#base-retry").isVisible(), false);
  page.once("dialog", (dialog) => dialog.accept());
  await page.locator("#base-archive").click();
  await page.waitForFunction(() =>
    document
      .getElementById("base-message")
      .textContent.includes("Archived with an unverified outcome"),
  );
  await draft("eth", "0.001");
  await page.getByRole("button", { name: "Cancel Base payment", exact: true }).click();
  // Exercise the browser cache lifecycle. Restoring Next must mount fresh
  // controls, with no opted-in address or phrase retained.
  const isNext = await page.locator('script[src*="/_next/"]').count();
  if (isNext) {
    await page.evaluate(() => {
      window.dispatchEvent(new PageTransitionEvent("pagehide", { persisted: true }));
    });
    assert.equal(await page.locator("#base-secret").count(), 0);
    await page.evaluate(() => {
      window.dispatchEvent(new PageTransitionEvent("pageshow", { persisted: true }));
    });
    await page
      .getByRole("button", { name: "Enable Base Sepolia", exact: true })
      .waitFor({ state: "visible" });
    console.log(
      "Next cache restoration: fresh disconnected Base controls restored without phrase or address",
    );
    assert.equal(await page.locator("#base-secret").inputValue(), "");
    await page.getByLabel("Base Sepolia RPC", { exact: true }).fill(fixture.url);
    await page.getByLabel("Your Zcash recovery phrase", { exact: true }).fill(words);
    await page.getByRole("button", { name: "Enable Base Sepolia", exact: true }).click();
    await page.waitForFunction(() =>
      document.getElementById("base-balance").textContent.includes("USDC"),
    );
  }
  await page.setViewportSize({ width: 390, height: 844 });
  assert.ok(
    await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth),
    "Combined mobile layout overflow",
  );
  console.log(
    `Real local EVM: ETH/USDC delivered, wrong phrase/network rejected, single sends, lost-ACK reload recovery and exact-byte retry after refusal passed (${page.url()})`,
  );
}
