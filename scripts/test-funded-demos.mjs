#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { cpSync, existsSync, mkdtempSync, readFileSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, sep } from 'node:path';
import * as playwright from 'playwright';
import { launchBrowser } from './browser-launch.mjs';
import { verifyReceiveQr } from './verify-receive-qr.mjs';
import { baseDemoFixture, exerciseCombinedBase } from './base-demo-fixture.mjs';
import { createExample } from './create-example.mjs';
import { regtestBrowserGateway } from './regtest-browser-gateway.mjs';
import { generate, zebraRpc } from './regtest-rpc.mjs';
import { assertLocalRegtestChain } from './sdk-harness.mjs';
import { ZEBRA_RPC } from './regtest-rpc.mjs';

const withBase = process.argv.includes('--with-base');
const baseOnly = process.argv.includes('--base-only');
const browserName = process.argv.find(arg => arg.startsWith('--browser='))?.slice(10) || 'chromium';
assert.ok(['chromium', 'firefox', 'webkit'].includes(browserName));
const root = resolve(import.meta.dirname, '..');
const scratch = mkdtempSync(join(tmpdir(), 'z-stack-funded-demos-'));
const vite = join(scratch, 'vite'), next = join(scratch, 'next');
const faucetWords = 'abandon '.repeat(11) + 'about';
const recipientWords = 'abandon '.repeat(23) + 'art';
const info = await zebraRpc('getblockchaininfo'); assertLocalRegtestChain(info, ZEBRA_RPC);
const nu63 = info.upgrades?.['37a5165b']?.activationheight || 1_000_000;
const nu7 = Object.values(info.upgrades ?? {}).find(u => u.name === 'NU7')?.activationheight;
const run = (command, args, cwd) => {
  const result = spawnSync(command, args, { cwd, stdio: 'inherit', shell: process.platform === 'win32',
    env: { ...process.env, NEXT_TELEMETRY_DISABLED: '1' } });
  assert.equal(result.status, 0, `${command} failed`);
};
for (const [name, app] of [['browser-wallet', vite], ['next-wallet', next]]) {
  createExample(name, app, join(root, 'artifacts'), {withBase});
  run('npm', ['install', '--ignore-scripts', '--no-audit', '--no-fund'], app);
}
const staticRoot = resolve(vite, 'dist');
const server = createServer((req, res) => {
  res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
  res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
  const pathname = new URL(req.url, 'http://localhost').pathname;
  const path = resolve(staticRoot, '.' + (pathname === '/' ? '/index.html' : pathname));
  if (!path.startsWith(staticRoot + sep)) { res.writeHead(403).end(); return; }
  try { res.setHeader('Content-Type', path.endsWith('.wasm') ? 'application/wasm' : path.endsWith('.js') ? 'application/javascript'
    : path.endsWith('.css') ? 'text/css' : path.endsWith('.svg') ? 'image/svg+xml' : 'text/html'); res.end(readFileSync(path)); }
  catch { res.writeHead(404).end(); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const viteOrigin = `http://127.0.0.1:${server.address().port}`;
const reservation = createServer();
await new Promise(resolve => reservation.listen(0,'127.0.0.1',resolve));
const nextPort = reservation.address().port;
await new Promise(resolve=>reservation.close(resolve));
const nextOrigin = `http://127.0.0.1:${nextPort}`;
const extraOrigins = ['http://localhost:5189', 'http://127.0.0.1:5189', 'http://localhost:4179', 'http://127.0.0.1:4179'];
const gateway = await regtestBrowserGateway([viteOrigin, nextOrigin, ...extraOrigins]);
const connection = `import { grpcWebTransport, type WalletOptions } from '@z-stack/sdk';
export const connection: WalletOptions = {network:'regtest', server:grpcWebTransport(${JSON.stringify(gateway.url)}, {transparent:true}),
regtestNu63Height:${nu63}, ${nu7 === undefined ? "" : `regtestNu7Height:${nu7},`} transparent:true, transparentScan:'compact', prewarmProvingKey:false};\n`;
writeFileSync(join(vite, 'src/connection.ts'), connection); writeFileSync(join(next, 'lib/connection.ts'), connection);
const originalIndex = readFileSync(join(vite, 'index.html'));
writeFileSync(join(vite, 'src/bootstrap.ts'), `import {createWallet, deriveAccount} from '@z-stack/sdk'; import {connection} from './connection';
const wallet=await createWallet({...connection, autoSync:false, autoShield:false, memoFetch:'on-demand', threads:2});
await wallet.restore(${JSON.stringify(faucetWords)}, {birthday:1});
Object.assign(window,{fixtureWallet:wallet, fixtureReady:true, fixtureRecipient:deriveAccount(${JSON.stringify(recipientWords)},'regtest').unifiedAddress});`);
writeFileSync(join(vite, 'index.html'), '<!doctype html><html><body><script type="module" src="/src/bootstrap.ts"></script></body></html>');
let browser, nextServer, baseFixture;
try {
  run('npm', ['run', 'build'], vite);
  browser = await launchBrowser(playwright[browserName]);
  if (withBase) baseFixture = await baseDemoFixture([viteOrigin, nextOrigin], faucetWords);
  const page = await browser.newPage();
  await page.route('**/*', route => { assert.equal(new URL(route.request().url()).hostname, '127.0.0.1', 'SDK demos must stay on loopback'); return route.continue(); });
  await page.goto(viteOrigin); await page.waitForFunction(() => window.fixtureReady, null, { timeout: 120_000 });
  async function engineSync() {
    const target = (await zebraRpc('getblockchaininfo')).blocks;
    for (let attempt = 0; attempt < 180; attempt++) {
      const height = await page.evaluate(async () => (await window.fixtureWallet.sync()).scannedHeight);
      if (height >= target) return;
      await page.waitForTimeout(500);
    }
    throw new Error('Fixture sync did not catch up');
  }
  await engineSync();
  const initial = await page.evaluate(async () => (await window.fixtureWallet.getWallet()).balance);
  assert.ok(initial.totalAvailable > 1_000_000, 'Fixture faucet needs mature funds');
  if (!baseOnly && initial.transparentAvailable >= 100_000) {
    await page.evaluate(async words => { await window.fixtureWallet.unlock(words); return window.fixtureWallet.shield(); }, faucetWords);
    await generate(15); await engineSync();
  }
  const recipient = await page.evaluate(() => window.fixtureRecipient);
  const funding = baseOnly ? {txid: undefined} : await page.evaluate(async ({ words, recipient }) => {
    await window.fixtureWallet.unlock(words); return window.fixtureWallet.send(recipient, '0.1', 'SDK regtest fixture');
  }, { words: faucetWords, recipient });
  if (!baseOnly) { assert.match(funding.txid, /^[a-f0-9]{64}$/); await generate(15); await engineSync(); }
  await page.evaluate(() => window.fixtureWallet.forget()); await page.evaluate(() => window.fixtureWallet.close());
  writeFileSync(join(vite, 'index.html'), originalIndex);
  run('npm', ['run', 'build'], vite); run('npm', ['run', 'build'], next);
  const bin = createRequire(join(next, 'package.json')).resolve('next/dist/bin/next');
  nextServer = spawn(process.execPath, [bin, 'start', '--hostname', '127.0.0.1', '--port', String(nextPort)], {cwd:next, stdio:'ignore'});
  for (let attempt=0; attempt<100; attempt++) { assert.equal(nextServer.exitCode,null,'Owned Next server exited before readiness'); try { if ((await fetch(nextOrigin)).ok) break; } catch {} await page.waitForTimeout(100); }
  async function syncUi(page, kind) {
    const before = await page.locator(kind === 'vite' ? '#history' : '.activity').innerText();
    await page.getByRole('button', {name:kind === 'vite' ? 'Sync' : 'Sync wallet', exact:true}).click();
    // Existing rows cannot be removed during an update.
    if (before) assert.equal(await page.locator(kind === 'vite' ? '#history' : '.activity').innerText(), before);
    await page.getByRole('button',{name:kind === 'vite' ? 'Sync' : 'Sync wallet',exact:true}).waitFor();
    await page.waitForFunction(() => !document.querySelector('#sync')?.disabled && ![...document.querySelectorAll('button')].some(button => button.textContent === 'Sync wallet' && button.disabled),null,{timeout:180_000});
  }
  for (const [kind, origin] of (process.argv.includes('--next-only') ? [['next', nextOrigin]] : [['vite', viteOrigin], ['next', nextOrigin]])) {
    const context = await browser.newContext(); const ui = await context.newPage(); const errors=[];
    ui.on('pageerror', error=>errors.push(error.message));
    await ui.route('**/*', route=> { assert.equal(new URL(route.request().url()).hostname,'127.0.0.1'); return route.continue(); });
    await ui.goto(origin); console.log(`Funded ${kind}: opening UI`);
    if (kind==='next') await ui.getByText('Restore an existing wallet',{exact:true}).click();
    await ui.getByLabel('Recovery phrase',{exact:true}).fill(faucetWords);
    const birthdayInput = ui.getByLabel(kind==='vite'?'Birthday height or date':'Wallet birthday',{exact:true});
    await birthdayInput.fill('2026-13-40');
    await ui.getByRole('button',{name:'Restore wallet',exact:true}).click();
    assert.equal(await ui.getByLabel('Recovery phrase',{exact:true}).inputValue(), faucetWords, 'Local birthday errors must retain the phrase');
    assert.equal(await birthdayInput.evaluate(input => input.validity.valid), false);
    await birthdayInput.fill('3');
    await ui.getByRole('button',{name:'Restore wallet',exact:true}).click();
    await ui.getByRole('button',{name:kind==='vite'?'Sync':'Sync wallet',exact:true}).waitFor();
    for(let attempt=0; attempt<100;attempt++){ if(await ui.getByRole('button',{name:kind==='vite'?'Sync':'Sync wallet',exact:true}).isEnabled())break; await ui.waitForTimeout(200); }
    await syncUi(ui,kind);
    const address=await ui.locator('#address').innerText(); assert.ok(address.startsWith('uregtest'));
    await verifyReceiveQr(ui, address);
    assert.equal((await ui.request.get(origin + '/favicon.svg')).status(), 200);
    await ui.getByLabel('Recipient address',{exact:true}).fill('zcash:' + address);
    await ui.getByLabel('Amount (ZEC)',{exact:true}).fill('0.0005');
    await ui.getByRole('button',{name:'Review payment',exact:true}).click();
    await ui.getByText(/This form does not accept zcash: payment links/).waitFor();
    let txid;
    for (const uncertain of baseOnly ? [] : [false, true]) {
    await ui.getByLabel('Recipient address',{exact:true}).fill(address);
    await ui.getByLabel('Amount (ZEC)',{exact:true}).fill('0.0005');
    await ui.getByLabel('Memo (optional)',{exact:true}).fill(`funded-${kind}-UI`);
    await ui.getByRole('button',{name:'Review payment',exact:true}).click();
    await ui.getByRole('button',{name:'Send 0.00050000 ZEC',exact:true}).waitFor({timeout:60_000});
    assert.ok((await ui.getByRole('region',{name:'Review payment',exact:true}).count()) || kind==='next');
    let staleRemoval;
    if (kind === 'vite' && !uncertain) {
      staleRemoval = await context.newPage();
      await staleRemoval.goto(origin);
      await staleRemoval.getByText(/Wallet opened · scanned through block/).waitFor({timeout:120_000});
      await staleRemoval.getByText('Remove local wallet',{exact:true}).click();
      await staleRemoval.getByLabel('I saved my recovery phrase and payment receipts',{exact:true}).check();
      assert.equal(await staleRemoval.getByRole('button',{name:'Remove from this browser',exact:true}).isEnabled(),true);
    }
    if (!uncertain) {
      await ui.getByLabel('Recovery phrase for this payment',{exact:true}).fill('not a phrase');
      await ui.getByRole('button',{name:'Send 0.00050000 ZEC',exact:true}).click();
      await ui.getByText('Those words are not a valid recovery phrase.',{exact:true}).waitFor({timeout:60_000});
      if (kind === 'vite') assert.equal(await ui.locator('#send-words').getAttribute('aria-invalid'), 'true');
    }
    await ui.getByLabel('Recovery phrase for this payment',{exact:true}).fill(faucetWords);
    if (kind === 'vite') assert.equal(await ui.locator('#send-words').getAttribute('aria-invalid'), null);
    if (uncertain) gateway.loseNextAcknowledgement();
    await ui.getByRole('button',{name:'Send 0.00050000 ZEC',exact:true}).click();
    assert.equal(await ui.locator('#send-words').inputValue(),'','Phrase must clear before awaiting proof');
    const txidElement=ui.locator(kind==='vite'?'#receipt-txid':'[data-testid=receipt-txid]');
    await txidElement.waitFor({timeout:300_000}); txid=await txidElement.innerText(); assert.match(txid,/^[a-f0-9]{64}$/);
    console.log(`Funded ${kind}: ${uncertain ? 'lost acknowledgement' : 'submitted'} receipt ${txid}`);
    if (uncertain) {
      console.log('Receipt state',kind,await ui.locator(kind==='vite'?'#receipt-state':'.receipt [data-testid=receipt-state]').innerText());
      await ui.getByText(/Submission not confirmed/).waitFor({timeout:30_000});
      assert.equal(await ui.getByRole('button',{name:'Review payment',exact:true}).isVisible().catch(()=>false),false, 'Unknown submission must block a second payment');
    }
    if (staleRemoval) {
      try {
        await staleRemoval.getByRole('button',{name:'Remove from this browser',exact:true}).click();
        await staleRemoval.locator('#remove-status').getByText('Sync to confirm or expire pending payments before removing this wallet.',{exact:true}).waitFor();
        assert.equal(await staleRemoval.getByRole('button',{name:'Remove from this browser',exact:true}).isEnabled(),false,'Stale tab must adopt the durable pending reservation and refuse deletion');
        assert.equal(await staleRemoval.locator('#address').innerText(),address,'Rejected removal must preserve identity');
        await staleRemoval.locator('#history').getByText(txid,{exact:false}).waitFor();
      } finally { await staleRemoval.close(); }
    }
    if (kind === 'vite') {
      await ui.getByText('Remove local wallet',{exact:true}).click();
      await ui.getByLabel('I saved my recovery phrase and payment receipts',{exact:true}).check();
      assert.equal(await ui.getByRole('button',{name:'Remove from this browser',exact:true}).isEnabled(),false,'Pending/uncertain payment must block removal');
      const reopened = await context.newPage();
      try {
        await reopened.goto(origin);
        await reopened.getByText(/Wallet opened · scanned through block/).waitFor({timeout:120_000});
        await reopened.getByText('Remove local wallet',{exact:true}).click();
        await reopened.getByLabel('I saved my recovery phrase and payment receipts',{exact:true}).check();
        assert.equal(await reopened.getByRole('button',{name:'Remove from this browser',exact:true}).isEnabled(),false,'Persisted pending payment must block removal after reopen');
      } finally { await reopened.close(); }
    }
    await generate(15); await ui.waitForTimeout(1500); await syncUi(ui,kind);
    await ui.getByText('Confirmed on-chain',{exact:true}).waitFor({timeout:180_000});
    if (kind === 'vite') {
      assert.equal(await ui.locator('#send-status').innerText(), 'Payment confirmed.', 'Confirmed receipt retained an uncertain-submission warning');
      assert.equal(await ui.getByRole('button',{name:'Remove from this browser',exact:true}).isEnabled(),true,'Confirmation must release the local removal guard');
      await ui.getByLabel('I saved my recovery phrase and payment receipts',{exact:true}).uncheck();
      await ui.getByText('Remove local wallet',{exact:true}).click();
    }
    if (!uncertain) await ui.getByRole('button',{name:'New payment',exact:true}).click();
    }
    await ui.getByText('Scan an earlier range',{exact:true}).click();
    await ui.getByLabel('Earlier height or date',{exact:true}).fill('1');
    await ui.getByLabel('I want to rebuild scan history',{exact:true}).check();
    let releaseTip, tipRequested;
    const tipHeld = new Promise(resolve => { releaseTip = resolve; });
    const tipStarted = new Promise(resolve => { tipRequested = resolve; });
    const holdTip = async route => {
      tipRequested(); await tipHeld; await route.continue();
    };
    if (kind === 'next') await ui.route('**/GetLatestBlock', holdTip);
    await ui.getByRole('button',{name:'Rescan wallet',exact:true}).click();
    if (kind === 'next') {
      try {
        await tipStarted;
        assert.equal(await ui.getByRole('button',{name:'Stop sync',exact:true}).count(), 0,
          'Stop sync must not be offered while the reset is preparing');
      } finally { releaseTip(); await ui.unroute('**/GetLatestBlock', holdTip); }
    }
    await ui.waitForFunction(() => !document.querySelector('#sync')?.disabled && ![...document.querySelectorAll('button')].some(button => button.textContent === 'Sync wallet' && button.disabled), null, {timeout:180_000});
    assert.equal(await ui.getByLabel('I want to rebuild scan history',{exact:true}).isChecked(), false, 'Each rescan needs a new confirmation');
    assert.equal(await ui.locator('#address').innerText(), address, 'Rescan must preserve receiving identity');
    if (txid) await ui.locator(kind==='vite'?'#history':'.activity').getByText(txid,{exact:kind==='next'}).waitFor({timeout:60_000});
    if (!baseOnly && txid) {
      await ui.getByRole('button',{name:'Load memos and details',exact:true}).click();
      const activity=ui.locator(kind==='vite'?'#history':'.activity');
      await activity.getByText(`funded-${kind}-UI`,{exact:true}).first().waitFor({timeout:60_000});
      await activity.getByText(/^Self send · Fee/).first().waitFor({timeout:60_000});
      assert.doesNotMatch(await activity.innerText(),/Sent · −0\.00010000/);
    }
    if (withBase) await exerciseCombinedBase(ui, baseFixture, faucetWords);
    await ui.setViewportSize({width:390,height:844});
    assert.equal(await ui.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false,'Mobile layout overflows');
    await ui.screenshot({path:join(scratch,`${kind}-mobile.png`),fullPage:true});
    await ui.reload(); await ui.getByRole('button',{name:kind==='vite'?'Sync':'Sync wallet',exact:true}).waitFor({timeout:120_000});
    if (txid) await ui.locator(kind==='vite'?'#history':'.activity').getByText(txid,{exact:kind==='next'}).waitFor({timeout:60_000});
    if (!baseOnly && txid) {
      // A separate browser profile proves classification from chain data, not
      // retained send metadata. Selective transaction retrieval stays explicit.
      const restoredContext = await browser.newContext();
      try {
        const restored = await restoredContext.newPage();
        let detailRequests = 0;
        restored.on('request', request => { if (request.url().includes('/GetTransaction')) detailRequests++; });
        await restored.goto(origin);
        if (kind === 'next') await restored.getByText('Restore an existing wallet',{exact:true}).click();
        await restored.getByLabel('Recovery phrase',{exact:true}).fill(faucetWords);
        await restored.getByLabel(kind==='vite'?'Birthday height or date':'Wallet birthday',{exact:true}).fill('1');
        await restored.getByRole('button',{name:'Restore wallet',exact:true}).click();
        await restored.waitForFunction(() => !document.querySelector('#sync')?.disabled &&
          [...document.querySelectorAll('button')].some(button => ['Sync','Sync wallet'].includes(button.textContent) && !button.disabled),null,{timeout:120_000});
        await syncUi(restored,kind);
        assert.equal(await restored.locator('#address').innerText(),address);
        const activity=restored.locator(kind==='vite'?'#history':'.activity');
        await activity.getByText(txid,{exact:kind==='next'}).waitFor({timeout:60_000});
        assert.equal(detailRequests,0,'Restore/sync silently fetched owned transaction IDs');
        await activity.getByText(/^Wallet change/).first().waitFor();
        await restored.getByRole('button',{name:'Load memos and details',exact:true}).click();
        await activity.getByText(/^Self send · Fee/).first().waitFor({timeout:60_000});
        await activity.getByText(`funded-${kind}-UI`,{exact:true}).first().waitFor();
        assert.ok(detailRequests>0,'Explicit detail retrieval did not query the light server');
        await restored.reload();
        await restored.locator(kind==='vite'?'#history':'.activity').getByText(/^Self send · Fee/).first().waitFor({timeout:60_000});
        console.log(`Funded ${kind}: fresh restore, explicit memos/details, self-send fee and reload passed`);
      } finally { await restoredContext.close(); }
    }
    assert.equal(errors.length,0,errors.join('\n')); await context.close();
    console.log(`Funded ${kind}: ${baseOnly ? "shared-wallet restore/sync/Base/reload/mobile" : "Zcash review/prove/pending/confirmed/reload/mobile"} passed`);
  }
  writeFileSync(join(scratch,'ACCEPTANCE.json'),JSON.stringify({sdkRevision:process.env.Z_STACK_REVISION||'working-tree',browser:browserName,baseTransfers:baseFixture?.hashes,nu63,nu7:nu7??null,gateway:gateway.url,fixtureRecipient:recipient,fixtureFundingTxid:funding.txid},null,2));
  console.log(`Funded demo evidence: ${scratch}`);
  if(process.argv.includes('--keep-gateway')) {
    console.log(`Regtest fixture gateway stays available: ${gateway.url}`);
    await new Promise(resolve=> { process.once('SIGTERM',resolve); process.once('SIGINT',resolve); });
  }
} finally {
  await baseFixture?.stop();
  nextServer?.kill('SIGTERM'); await browser?.close(); await gateway.stop();
  server.closeAllConnections(); await new Promise(resolve=>server.close(resolve));
  // Keep evidence, not a new pair of node_modules trees after every browser run.
  if (!process.argv.includes('--keep-consumers')) for (const app of [vite, next]) rmSync(app, {recursive:true, force:true});
  console.log(`Funded evidence retained: ${scratch}${process.argv.includes('--keep-consumers') ? ' (including consumers)' : ''}`);
}
