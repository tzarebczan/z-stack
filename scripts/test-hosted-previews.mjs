import assert from 'node:assert/strict';
import { chromium } from 'playwright';
// Disposable testnet creation only; never use a funded user profile.
const [vite, next, ...extra] = process.argv.slice(2);
assert.ok(vite && next && extra.length === 0,
  'Usage: node scripts/test-hosted-previews.mjs <vite-origin> <next-origin>');
function previewOrigin(value) {
  const url = new URL(value);
  assert.ok(url.protocol === 'https:' || (url.protocol === 'http:' &&
    ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)),
    'Preview origins must use HTTPS or loopback HTTP');
  assert.ok(!url.username && !url.password && !url.search && !url.hash && url.pathname === '/',
    'Provide a bare preview origin without credentials, path, query or fragment');
  return url.origin;
}
const origins = { vite: previewOrigin(vite), next: previewOrigin(next) };
const browser=await chromium.launch();
try {for(const [kind,origin] of Object.entries(origins)){
 const response=await fetch(origin);assert.equal(response.status,200);assert.equal(response.headers.get('cross-origin-opener-policy'),'same-origin');assert.equal(response.headers.get('cross-origin-embedder-policy'),'require-corp');
 assert.ok(response.headers.get('content-security-policy')?.includes("'wasm-unsafe-eval'"));
 const info=await (await fetch(origin+'/build-info.json')).json();assert.equal(info.network,'testnet');
 const context=await browser.newContext(),page=await context.newPage();const errors=[],engines=[];
 page.on('pageerror',e=>errors.push(e.message));page.on('response',r=>{if(r.url().endsWith('.wasm'))engines.push(r.status());});
 await page.goto(origin);await page.getByRole('button',{name:'Create wallet',exact:true}).waitFor({timeout:120_000});
 await page.getByRole('button',{name:'Create wallet',exact:true}).click();
 const saved=page.getByRole('button',{name:kind==='vite'?'I saved these words — finish':'Done, hide phrase',exact:true});
 if(kind==='next') await page.getByRole('checkbox').check();
 await saved.click({timeout:120_000});
 await page.locator('#address').waitFor({timeout:120_000});assert.ok((await page.locator('#address').innerText()).startsWith('utest'));
 assert.equal(await page.evaluate(()=>crossOriginIsolated),true);assert.ok(engines.length&&engines.every(s=>s===200));
 await page.setViewportSize({width:390,height:844});assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false);
 assert.deepEqual(errors,[]);await context.close();console.log(`${kind}: hosted testnet engine/create/recovery/isolation/mobile passed at ${origin}`);
}} finally {await browser.close();}
