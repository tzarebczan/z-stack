import assert from "node:assert/strict";
import { createServer } from "node:http";
import {
  mkdtempSync,
  readFileSync,
  writeFileSync,
  existsSync,
  rmSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { pathToFileURL } from "node:url";
const root = resolve(import.meta.dirname, "..");
const version = JSON.parse(
  readFileSync(join(root, "packages/sdk/package.json"), "utf8"),
).version;
const { createGrpcWebProxy } = await import(
  pathToFileURL(join(root, "scripts/grpc-web-proxy.mjs"))
);
const playwright = await import("playwright");
const { launchBrowser } = await import("./browser-launch.mjs");
const browserName =
  process.argv.find((arg) => arg.startsWith("--browser="))?.slice(10) ??
  "chromium";
assert(["chromium", "firefox", "webkit"].includes(browserName));
assert(
  process.env.Z_STACK_GIFT_FIXTURE_STATE,
  "Set Z_STACK_GIFT_FIXTURE_STATE to an owned disposable regtest fixture descriptor (lightUrl, rpcUrl, cookiePath).",
);
const fixture = JSON.parse(
  readFileSync(process.env.Z_STACK_GIFT_FIXTURE_STATE),
);
for (const url of [fixture.lightUrl, fixture.rpcUrl])
  assert(
    ["127.0.0.1", "localhost"].includes(new URL(url).hostname),
    "Gift qualification only uses loopback",
  );
const auth =
  "Basic " +
  Buffer.from(readFileSync(fixture.cookiePath, "utf8").trim()).toString(
    "base64",
  );
async function rpc(method, params = []) {
  const r = await fetch(fixture.rpcUrl, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: auth },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
  assert(r.ok);
  const j = await r.json();
  if (j.error) throw new Error("fixture RPC failed");
  return j.result;
}
const { assertLocalRegtestChain } = await import("./sdk-harness.mjs");
assertLocalRegtestChain(await rpc("getblockchaininfo"), fixture.rpcUrl);
assert.equal(
  await rpc("getblockhash", [0]),
  "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327",
  "Fixture genesis must match regtest",
);
const nu63 = Number(process.env.Z_STACK_REGTEST_NU6_3 || 1_000_000);
const nu7 =
  process.env.Z_STACK_REGTEST_NU7 === undefined
    ? undefined
    : Number(process.env.Z_STACK_REGTEST_NU7);
const scratch = mkdtempSync(join(tmpdir(), "z-stack-gift-funded-"));
const run = (cmd, args, cwd = root) => {
  const r = spawnSync(cmd, args, { cwd, stdio: "inherit" });
  assert.equal(r.status, 0, cmd + " failed");
};
if (!existsSync(join(scratch, `archives/z-stack-sdk-${version}.tgz`)))
  run("node", ["scripts/pack-sdk.mjs", join(scratch, "archives")]);
if (!existsSync(join(scratch, `archives/z-stack-gift-cards-${version}.tgz`)))
  run("node", [
    "scripts/pack-sdk.mjs",
    join(scratch, "archives"),
    "--gift-cards-only",
  ]);
const app = join(scratch, "app");
run("node", [
  "scripts/create-example.mjs",
  "browser-wallet",
  app,
  "--archives",
  join(scratch, "archives"),
  "--with-gift-cards",
  "--install",
]);
let loseAck = false,
  submitCount = 0;
const staticRoot = join(app, "dist");
const web = createServer((req, res) => {
  const f = resolve(
    staticRoot,
    "." +
      (req.url === "/"
        ? "/index.html"
        : new URL(req.url, "http://localhost").pathname),
  );
  if (!f.startsWith(staticRoot + "/") || !existsSync(f)) {
    res.writeHead(404).end();
    return;
  }
  res
    .writeHead(200, {
      "Content-Type": f.endsWith(".wasm")
        ? "application/wasm"
        : f.endsWith(".js")
          ? "text/javascript"
          : "text/html",
      "Cross-Origin-Opener-Policy": "same-origin",
      "Cross-Origin-Embedder-Policy": "credentialless",
    })
    .end(readFileSync(f));
});
await new Promise((r) => web.listen(0, "127.0.0.1", r));
const origin = "http://127.0.0.1:" + web.address().port;
const gateway = createGrpcWebProxy({
  upstream: fixture.lightUrl,
  origins: [origin],
  allowTransparent: true,
});
const forward = gateway.listeners("request")[0];
gateway.removeAllListeners("request");
gateway.on("request", async (req, res) => {
  if (!req.url.endsWith("/SendTransaction") || req.method === "OPTIONS") {
    forward(req, res);
    return;
  }
  try {
    const chunks = [];
    for await (const c of req) chunks.push(c);
    const b = Buffer.concat(chunks);
    let pos = 6,
      len = 0,
      shift = 0;
    for (;;) {
      const x = b[pos++];
      len += (x & 127) * 2 ** shift;
      if (!(x & 128)) break;
      shift += 7;
      assert(shift < 28);
    }
    const txid = await rpc("sendrawtransaction", [
      b.subarray(pos, pos + len).toString("hex"),
    ]);
    submitCount++;
    const h = {
      "access-control-allow-origin": origin,
      "content-type": "application/grpc-web+proto",
    };
    if (loseAck) {
      loseAck = false;
      res.writeHead(502, h).end();
      return;
    }
    const m = Buffer.concat([Buffer.from([18, 64]), Buffer.from(txid)]),
      p = Buffer.alloc(5);
    p.writeUInt32BE(m.length, 1);
    const tr = Buffer.from("grpc-status:0\r\n"),
      q = Buffer.alloc(5);
    q[0] = 128;
    q.writeUInt32BE(tr.length, 1);
    res.writeHead(200, h).end(Buffer.concat([p, m, q, tr]));
  } catch {
    res.writeHead(502, { "access-control-allow-origin": origin }).end();
  }
});
await new Promise((r) => gateway.listen(0, "127.0.0.1", r));
const url = "http://127.0.0.1:" + gateway.address().port;
writeFileSync(
  join(app, "index.html"),
  '<!doctype html><html><body><script type="module" src="/src/gift-funded.ts"></script></body></html>',
);
writeFileSync(
  join(app, "src/gift-funded.ts"),
  `
import {createWallet,indexedDbWalletStorage,grpcWebTransport} from '@z-stack/sdk';
import {createGiftCards,createEncryptedGiftStore} from '@z-stack/gift-cards';
import {createBrowserGiftBackend} from '@z-stack/gift-cards/browser';
const words='abandon '.repeat(11)+'about';
const main=await createWallet({network:'regtest',server:grpcWebTransport(${JSON.stringify(url)},{transparent:true}),storage:indexedDbWalletStorage({name:'gift-funded-main'}),regtestNu63Height:${nu63},${nu7 === undefined ? "" : `regtestNu7Height:${nu7},`}autoShield:false,autoSync:false,transparent:true,transparentScan:'compact',threads:1,preferMulticore:false,prewarmProvingKey:false,deepSync:true});
if(!await main.load())await main.restore(words,{birthday:1});
const material=await crypto.subtle.importKey('raw',new TextEncoder().encode(words),'HKDF',false,['deriveKey']);
const key=await crypto.subtle.deriveKey({name:'HKDF',hash:'SHA-256',salt:new TextEncoder().encode('publicfixture'),info:new TextEncoder().encode('gift-vault')},material,{name:'AES-GCM',length:256},false,['encrypt','decrypt']);
const backend=createBrowserGiftBackend({network:'regtest',server:${JSON.stringify(url)},namespace:'gift-funded-isolated',regtestNu63Height:${nu63},${nu7 === undefined ? "" : `regtestNu7Height:${nu7},`}deepSync:true});
const store=createEncryptedGiftStore({name:'gift-funded-journal',key});
const gifts=createGiftCards({network:'regtest',shareEndpoint:'https://wallet.example/gift',backend,store,source:{tip:async()=> (await main.tip()).tip,estimateFee:(to,amount)=>main.estimateFee(to,amount),send:async(to,amount,options)=>{await main.unlock(words);return main.send(to,amount,undefined,options);}}});
Object.assign(window,{f:{main,gifts,backend,words}});
`,
);
run("npm", ["run", "build"], app);
let browser;
try {
  browser = await launchBrowser(playwright[browserName]);
  const page = await browser.newPage();
  page.on("pageerror", (e) => console.error("pageerror", e.message));
  await page.goto(origin);
  await page.waitForFunction(() => window.f, { timeout: 120000 });
  const sync = async () => {
    const tip = await rpc("getblockcount");
    for (let i = 0; i < 120; i++) {
      const h = await page.evaluate(
        async () => (await window.f.main.sync()).scannedHeight,
      );
      if (h >= tip) return;
      await new Promise((r) => setTimeout(r, 500));
    }
    throw new Error("sync timeout");
  };
  await sync();
  const balance = await page.evaluate(
    async () => (await window.f.main.getWallet()).balance,
  );
  console.log("faucet mature", balance.totalAvailable > 1000000);
  if (
    balance.orchardAvailable +
      balance.ironwoodAvailable +
      balance.saplingAvailable <
    500000
  ) {
    await page.evaluate(async () => {
      const f = window.f;
      await f.main.unlock(f.words);
      await f.main.shield();
    });
    await rpc("generate", [15]);
    await sync();
  }
  const before = await page.evaluate(
    async () => (await window.f.main.getWallet()).unifiedAddress,
  );
  console.log("Creating and funding an isolated gift; simulating a lost acknowledgement");
  const review = await page.evaluate(async () => {
    const r = await window.f.gifts.prepare({
      amountZat: 50000n,
      label: "Fixture gift",
    });
    return { id: r.record.id, fee: r.feeZat.toString() };
  });
  loseAck = true;
  assert.equal(
    await page.evaluate(async (r) => {
      try {
        await window.f.gifts.fund(r.id, { maxFeeZat: BigInt(r.fee) });
        return "unexpected";
      } catch (e) {
        return e.code;
      }
    }, review),
    "unresolved",
  );
  const afterFunding = submitCount;
  console.log("Funding receipt retained; reopening the browser wallet");
  await page.reload();
  await page.waitForFunction(() => window.f, { timeout: 120000 });
  assert.equal(
    await page.evaluate(async (r) => {
      try {
        await window.f.gifts.fund(r.id, { maxFeeZat: BigInt(r.fee) });
        return "unexpected";
      } catch (e) {
        return e.code;
      }
    }, review),
    "unresolved",
  );
  assert.equal(submitCount, afterFunding);
  await rpc("generate", [8]);
  await sync();
  let inspected;
  for (let i = 0; i < 80; i++) {
    inspected = await page.evaluate(async (id) => {
      const s = await window.f.gifts.inspect(id);
      return {
        phase: s.record.phase,
        confirmations: s.confirmations,
        available: s.availableZat.toString(),
      };
    }, review.id);
    if (inspected.confirmations >= 6) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  assert.equal(inspected.confirmations, 6);
  assert.equal(inspected.phase, "funded");
  assert.equal(inspected.available, "60000");
  console.log("Exact face amount and reserve confirmed through six scanned blocks");
  const redeemReview = await page.evaluate(
    async ({ id, destination }) => {
      const s = await window.f.gifts.prepareRedeem(id, destination);
      return { fee: s.feeZat.toString(), amount: s.amountZat.toString() };
    },
    { id: review.id, destination: before },
  );
  assert.equal(redeemReview.amount, "50000");
  const claim = await page.evaluate(
    async ({ id, destination, fee }) =>
      window.f.gifts.redeem(id, { destination, maxFeeZat: BigInt(fee) }),
    { id: review.id, destination: before, fee: redeemReview.fee },
  );
  assert.match(claim.claimTxid, /^[a-f0-9]{64}$/);
  const afterClaim = submitCount;
  assert.equal(
    await page.evaluate(
      async ({ id, destination, fee }) => {
        try {
          await window.f.gifts.redeem(id, {
            destination,
            maxFeeZat: BigInt(fee),
          });
          return "unexpected";
        } catch (e) {
          return e.code;
        }
      },
      { id: review.id, destination: before, fee: redeemReview.fee },
    ),
    "unresolved",
  );
  assert.equal(submitCount, afterClaim);
  const claimTip = await rpc("getblockcount");
  await rpc("generate", [8]);
  await sync();
  const final = await page.evaluate(async (id) => {
    const s = await window.f.gifts.inspect(id);
    return { phase: s.record.phase, available: s.availableZat.toString() };
  }, review.id);
  assert.equal(final.phase, "redeemed");
  assert.equal(final.available, "0");
  console.log("Claim mined; invalidating its block to test recovery");
  assert.equal(
    await page.evaluate(
      async () => (await window.f.main.getWallet()).unifiedAddress,
    ),
    before,
  );
  const claimBlock = await rpc("getblockhash", [claimTip + 1]);
  await rpc("invalidateblock", [claimBlock]);
  const rewoundTip = await rpc("getblockcount");
  for (let i = 0; i < 120; i++) {
    const lightTip = await page.evaluate(
      async () => (await window.f.main.tip()).tip,
    );
    if (lightTip === rewoundTip) break;
    if (i === 119)
      throw new Error("light server did not follow invalidated block");
    await new Promise((r) => setTimeout(r, 500));
  }
  const reorg = await page.evaluate(async (id) => {
    const s = await window.f.gifts.inspect(id);
    return {
      phase: s.record.phase,
      claimTxid: s.record.claimTxid,
      scanned: s.snapshot.scannedHeight,
    };
  }, review.id);
  assert.equal(reorg.phase, "redeeming");
  assert.equal(reorg.claimTxid, claim.claimTxid);
  assert.equal(reorg.scanned, rewoundTip);
  console.log("Reorged claim returned to pending; checking the same signed claim after re-mining");
  assert.equal(
    await page.evaluate(
      async ({ id, destination, fee }) => {
        try {
          await window.f.gifts.redeem(id, {
            destination,
            maxFeeZat: BigInt(fee),
          });
          return "unexpected";
        } catch (e) {
          return e.code;
        }
      },
      { id: review.id, destination: before, fee: redeemReview.fee },
    ),
    "unresolved",
  );
  await rpc("generate", [8]);
  await sync();
  let settled;
  for (let i = 0; i < 80; i++) {
    settled = await page.evaluate(async (id) => {
      const s = await window.f.gifts.inspect(id);
      return { phase: s.record.phase, claimTxid: s.record.claimTxid };
    }, review.id);
    if (settled.phase === "redeemed") break;
    await new Promise((r) => setTimeout(r, 500));
  }
  assert.equal(settled.phase, "redeemed");
  assert.equal(settled.claimTxid, claim.claimTxid);
  console.log(
    JSON.stringify({
      browser: browserName,
      reorgRecovery: true,
      fundedBrowserGift: true,
      lostFundingAcknowledgementRecovered: true,
      reload: true,
      exactFace: 50000,
      reserve: 10000,
      noDuplicateFunding: true,
      noDuplicateClaim: true,
      minedClaim: true,
      preservedMainWallet: true,
    }),
  );
} finally {
  await browser?.close();
  await new Promise((r) => {
    web.closeAllConnections();
    web.close(r);
  });
  await new Promise((r) => {
    gateway.closeAllConnections();
    gateway.close(r);
  });
  rmSync(scratch, { recursive: true, force: true });
}
