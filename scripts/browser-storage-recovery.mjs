#!/usr/bin/env node
// Requires Playwright (or Z_STACK_PLAYWRIGHT_MODULE=/path/to/playwright/index.mjs).
// Optional Z_<BROWSER>_EXECUTABLE overrides permit installed browser revisions.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {mkdtempSync,rmSync} from 'node:fs';
const {chromium,firefox,webkit}=await import(process.env.Z_STACK_PLAYWRIGHT_MODULE ?? 'playwright');
import {createServer} from '../packages/web/node_modules/vite/dist/node/index.js';
import {fileURLToPath} from 'node:url';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
const root=fileURLToPath(new URL('../packages/web',import.meta.url));
const origin='http://127.0.0.1:15182';
const vite=await createServer({root,configFile:`${root}/vite.config.ts`,server:{host:'127.0.0.1',port:15182,strictPort:true,hmr:false,watch:null}});await vite.listen();
try{
 for(const browserType of [chromium,firefox,webkit]){
  const name=browserType.name();
  const profile=mkdtempSync(join(tmpdir(),'zrecovery-'));
  const executablePath=process.env[`Z_${name.toUpperCase()}_EXECUTABLE`];
  let ctx;
  const open=async()=>{ctx=await browserType.launchPersistentContext(profile,{headless:true,executablePath,viewport:{width:390,height:844}});await ctx.route('**/*',r=>new URL(r.request().url()).origin===origin?r.continue():r.abort());const p=await ctx.newPage();await p.goto(`${origin}/test/snapshot-recovery.html`);await p.waitForFunction(()=>!!window.snapshotRecovery);return p;};
  try{
   let page=await open();const checksum=await page.evaluate(()=>window.snapshotRecovery.prepare());
   for(const fault of ['quota','cancel'])assert.equal(await page.evaluate(f=>window.snapshotRecovery.replacement(f),fault),checksum);
   await page.evaluate(()=>{window.snapshotRecovery.replacement('terminate').catch(()=>{});});
   await page.waitForFunction(()=>document.body.dataset.held==='true');
   await ctx.close();page=await open();
   assert.equal(await page.evaluate(()=>window.snapshotRecovery.verify()),checksum);
   console.log(JSON.stringify({browser:name,quotaRollback:true,cancelRollback:true,terminatedWriteRestart:true,bytes:8_000_000}));
  }finally{await ctx?.close();rmSync(profile,{recursive:true,force:true});}
 }
const profile=mkdtempSync(join(tmpdir(),'zbrowser-kill-'));let child,browser;
async function open(){
 child=spawn(process.env.Z_CHROMIUM_EXECUTABLE ?? chromium.executablePath(),['--headless','--no-sandbox','--disable-dev-shm-usage','--remote-debugging-port=0',`--user-data-dir=${profile}`,'about:blank'],{stdio:['ignore','ignore','pipe']});
 const endpoint=await new Promise((res,rej)=>{let output='';const timer=setTimeout(()=>rej(new Error('debugger startup timeout')),15000);child.stderr.on('data',data=>{output+=data;const m=/DevTools listening on (ws:\/\/[^\s]+)/.exec(output);if(m){clearTimeout(timer);res(m[1]);}});child.once('exit',()=>{clearTimeout(timer);rej(new Error('browser exited'));});});
 browser=await chromium.connectOverCDP(endpoint);const context=browser.contexts()[0];
 await context.route('**/*',r=>new URL(r.request().url()).origin==='http://127.0.0.1:15182'?r.continue():r.abort());
 const page=await context.newPage();await page.goto('http://127.0.0.1:15182/test/snapshot-recovery.html');await page.waitForFunction(()=>!!window.snapshotRecovery);return page;
}
try{let page=await open();const hash=await page.evaluate(()=>window.snapshotRecovery.prepare());await page.evaluate(()=>{window.snapshotRecovery.replacement('terminate').catch(()=>{});});await page.waitForFunction(()=>document.body.dataset.held==='true');const exited=once(child,'exit');child.kill('SIGKILL');await exited;await browser.close().catch(()=>{});page=await open();assert.equal(await page.evaluate(()=>window.snapshotRecovery.verify()),hash);console.log(JSON.stringify({browser:'chromium',processSigkillDuringWrite:true,exactSnapshotRecovered:true}));}finally{await browser?.close();child?.kill('SIGTERM');rmSync(profile,{recursive:true,force:true});}

}finally{await vite.close();}
