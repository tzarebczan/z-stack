// Disposable public PIR protocol proof. No wallet data, production service or node.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { readFile, mkdir, writeFile } from 'node:fs/promises';
import { resolve, join, sep } from 'node:path';
import { once } from 'node:events';
import { chromium, firefox } from 'playwright';
import ts from 'typescript';
const root = resolve(import.meta.dirname, '..');
const output = resolve(process.argv[2] ?? '/tmp/coffer-enhance-wallet-interop');
const packages={st:resolve(root,process.env.ENHANCE_ST_DIR??'packages/sdk/src/generated'),mt:resolve(root,process.env.ENHANCE_MT_DIR??'packages/sdk/src/generated-mt')};
const binary = process.env.ENHANCE_FIXTURE_BIN ?? '/home/tom/.cache/coffer-enhance-sdk-target/debug/examples/enhance_wallet_fixture';
const child = spawn(binary, [], { cwd: root, env: { ...process.env, RAYON_NUM_THREADS: '2' }, stdio: ['ignore', 'pipe', 'pipe'] });
const deadline = (promise, ms, message) => new Promise((yes, no) => {
  const timer = setTimeout(() => no(new Error(message)), ms);
  promise.then(yes, no).finally(() => clearTimeout(timer));
});
let childExited = false;
const exited = once(child, 'exit').then(() => { childExited = true; });
let server;
let browser;
const worker = `
self.onmessage = async ({data:{mode}}) => {
 try {
  const mod=await import('/'+mode+'/z_wasm.js');await mod.default();if(mode==='mt')await mod.initThreadPool(2);
  const threads=mod.threadCount();if(threads!==(mode==='mt'?2:1))throw new Error('wrong thread count');
  const {syncPrivateEnhance}=await import('/host-driver.mjs');
  const meta=await(await fetch('/metadata')).json();const encode=v=>new TextEncoder().encode(JSON.stringify(v));
  const blob=Uint8Array.from(atob(meta.blob),c=>c.charCodeAt(0));
  const check=(value,msg)=>{if(!value)throw new Error(msg);};
  const rejects=async(fn,code)=>{try{await fn();}catch(e){check(String(e).includes(code),'wrong refusal '+String(e));return;}throw new Error('missing refusal '+code);};
  const create=()=>{const w=mod.WasmWallet.fromUfvk('mainnet',meta.ufvk,meta.height,0);w.applyTreeState(JSON.stringify(meta.tree));w.applyCompactBlocksSummary(blob);return w;};
  const wallet=create();const before=wallet.toSnapshot();let saved;let requests=[];let outside=0;
  const source={privateEnhance:async(op,bytes,time)=>wallet.privateEnhance(op,bytes,time)};
  const transport={exchange:async(request,signal)=>{
   requests.push(request.kind);
   if(request.kind==='init')return{status:200,body:encode(meta.manifest)};
   if(request.kind==='session'){check(request.sessionId===meta.material.session_id,'wrong public session');return{status:200,body:encode(meta.material)};}
   check(request.kind==='query','unknown operation');check(request.anchor.height===meta.height&&request.anchor.hash===meta.hash,'wrong accepted query anchor');
   return{status:200,body:new Uint8Array(await(await fetch('/query',{method:'POST',body:request.body,signal})).arrayBuffer())};
  }};
  const run=(extra={})=>syncPrivateEnhance({source,transport,signal:new AbortController().signal,assertCurrent:()=>{},checkpoint:async()=>{saved=wallet.toSnapshot();},...extra});
  // A real query has reached the provider; cancellation must retire Rust before its late ciphertext resolves.
  const controller=new AbortController();let entered,release;const pending=new Promise(r=>entered=r);
  const delayed={exchange:async(request,signal)=>{
   if(request.kind!=='query')return transport.exchange(request,signal);
   const reply=await transport.exchange(request,new AbortController().signal);entered();return new Promise(resolve=>{release=()=>resolve(reply);});
  }};
  const canceled=run({transport:delayed,signal:controller.signal});await pending;controller.abort(new Error('owned cancel'));
  await rejects(()=>canceled,'owned cancel');release();await new Promise(r=>setTimeout(r,0));
  check(wallet.toSnapshot().every((v,i)=>v===before[i]),'late canceled response changed wallet');
  await rejects(()=>wallet.privateEnhance('finish',new Uint8Array(30836),Math.floor(Date.now()/1000)),'enhance_context_consumed');
  const count=await run();check(count===1,'real wallet record not applied');
  const history=JSON.parse(wallet.history(10));check(history.length===1&&history[0].memos.includes('private hello'),'authenticated memo missing');
  check(saved instanceof Uint8Array,'Rust apply not checkpointed');
  const restored=mod.WasmWallet.fromSnapshot(saved);check(JSON.parse(restored.history(10))[0].memos.includes('private hello'),'memo lost after reload');
  const calls=requests.length;check(await run()===0&&requests.length===calls,'resolved action produced provider I/O');
  const malformed=mod.WasmWallet.fromSnapshot(before);const time=Math.floor(Date.now()/1000);
  malformed.privateEnhance('begin',encode(meta.manifest),time);malformed.privateEnhance('material',encode(meta.material),time);malformed.privateEnhance('prepare',new Uint8Array(),time);
  await rejects(()=>malformed.privateEnhance('finish',new Uint8Array([1]),time),'enhance_reply_invalid');
  await rejects(()=>malformed.privateEnhance('finish',new Uint8Array([1]),time),'enhance_context_consumed');
  check(malformed.toSnapshot().every((v,i)=>v===before[i]),'malformed reply changed wallet');
  const stale=mod.WasmWallet.fromSnapshot(before);stale.privateEnhance('begin',encode(meta.manifest),time);stale.privateEnhance('material',encode(meta.material),time);
  const ciphertext=stale.privateEnhance('prepare',new Uint8Array(),time);const reply=new Uint8Array(await(await fetch('/query',{method:'POST',body:ciphertext})).arrayBuffer());
  stale.resetScan();await rejects(()=>stale.privateEnhance('finish',reply,time),'enhance_anchor_unavailable');
  await rejects(()=>stale.privateEnhance('finish',reply,time),'enhance_context_consumed');
  const expired=mod.WasmWallet.fromSnapshot(before);expired.privateEnhance('begin',encode(meta.manifest),100);expired.privateEnhance('material',encode(meta.material),100);expired.privateEnhance('prepare',new Uint8Array(),100);
  await rejects(()=>expired.privateEnhance('finish',reply,130),'enhance_routing_refresh_required');
  await rejects(()=>expired.privateEnhance('finish',reply,130),'enhance_context_consumed');
  for(const w of[wallet,restored,malformed,stale,expired])w.free();
  self.postMessage({ok:true,mode,threads,queries:requests.filter(r=>r==='query').length+1,checks:['genuine-compact-note-scan','SDK-driver-native-PIR-query','authenticated-653B-memo-apply','checkpoint-and-reload','resolved-zero-provider-I/O','cancel-before-late-ciphertext','malformed-consumes-preserves-wallet','reset-wins-late-reply','expired-reply-consumes'],outside});
 }catch(error){self.postMessage({ok:false,error:String(error)});}
};`;
const main = `const mode=location.pathname.split('/')[1];const worker=new Worker('/runner-worker.mjs',{type:'module'});worker.onmessage=({data})=>{window.result=data;worker.terminate();};worker.postMessage({mode});`;
try {
 const port = await deadline(new Promise((yes,no)=> {
  let buffer='';child.stdout.on('data',chunk=>{buffer+=chunk.toString();if(buffer.includes('\n')) {try{yes(JSON.parse(buffer.split('\n')[0]).port);}catch(e){no(e);}}});child.once('error',no);child.once('exit',()=>no(new Error('fixture exited before ready')));
 }),120000,'fixture startup deadline');
 assert.ok(Number.isInteger(port)&&port>0&&port<=65535);
 server=createServer(async(req,res)=> {
  try {
   res.setHeader('Cross-Origin-Opener-Policy','same-origin');res.setHeader('Cross-Origin-Embedder-Policy','require-corp');res.setHeader('Cross-Origin-Resource-Policy','same-origin');
   res.setHeader('Cache-Control','no-store');res.setHeader('Content-Security-Policy',"default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; worker-src 'self'; img-src 'self'");
   const url=new URL(req.url,'http://127.0.0.1');
   if(url.pathname==='/metadata'||url.pathname==='/query') {
    const chunks=[];let length=0;
    for await(const chunk of req){length+=chunk.length;if(length>282740)throw new Error('fixture body bound');chunks.push(chunk);}
    const reply=await fetch('http://127.0.0.1:'+port+url.pathname,{method:req.method,body:req.method==='POST'?Buffer.concat(chunks):undefined,signal:AbortSignal.timeout(120000)});
    const bytes=new Uint8Array(await reply.arrayBuffer());assert.ok(bytes.length<=262144);
    res.writeHead(reply.status,{'Content-Type':url.pathname==='/metadata'?'application/json':'application/octet-stream'}).end(bytes);return;
   }
   if(url.pathname==='/host-driver.mjs'||url.pathname==='/abort.mjs'){const name=url.pathname==='/host-driver.mjs'?'private-enhance':'abort';const source=await readFile(join(root,'packages/sdk/src',name+'.ts'),'utf8');const js=ts.transpileModule(source,{compilerOptions:{target:ts.ScriptTarget.ES2022,module:ts.ModuleKind.ESNext}}).outputText.replace('./abort','/abort.mjs');res.writeHead(200,{'Content-Type':'application/javascript'}).end(js);return;}
   if(url.pathname==='/favicon.ico'){res.writeHead(204).end();return;}
   if(url.pathname==='/runner-worker.mjs'){res.writeHead(200,{'Content-Type':'application/javascript'}).end(worker);return;}
   if(url.pathname==='/main.mjs'){res.writeHead(200,{'Content-Type':'application/javascript'}).end(main);return;}
   const match=/^\/(st|mt)\/(.*)$/.exec(url.pathname);
   if(!match){res.writeHead(404).end();return;}
   if(match[2]==='test'){res.writeHead(200,{'Content-Type':'text/html'}).end('<!doctype html><script type="module" src="/main.mjs"></script>');return;}
   const base=packages[match[1]];
   const path=resolve(base,match[2]||'z_wasm.js');assert.ok(path.startsWith(base+sep));
   const bytes=await readFile(path);res.writeHead(200,{'Content-Type':path.endsWith('.wasm')?'application/wasm':path.endsWith('.json')?'application/json':'application/javascript'}).end(bytes);
  }catch{res.writeHead(400).end();}
 });
 server.listen(0,'127.0.0.1');await once(server,'listening');
 const origin='http://127.0.0.1:'+server.address().port;
 const reports=[];
 for(const [name,type] of Object.entries({chromium,firefox})) {
  browser=await type.launch({headless:true});
  for(const mode of ['st','mt']) {
   const context=await browser.newContext();
   try {
    const page=await context.newPage();const errors=[],requests=[];
    page.on('pageerror',e=>errors.push(e.message));page.on('console',m=>{if(m.type()==='error'||m.type()==='warning')errors.push(m.text());});
    context.on('request',r=>{requests.push(r.url());assert.equal(new URL(r.url()).origin,origin,'outside fixture request');});
    await page.goto(origin+'/'+mode+'/test');
    await page.waitForFunction(()=>window.result!==undefined,null,{timeout:180000});
    const result=await page.evaluate(()=>window.result);assert.equal(result.ok,true,JSON.stringify(result));assert.deepEqual(errors,[]);
    reports.push({browser:name,version:browser.version(),...result,requests:requests.length});
    console.log(name+' '+mode+': real wallet SDK/PIR authentication and lifecycle passed');
   }finally{await context.close();}
  }
  await browser.close();browser=undefined;
 }
 await mkdir(output,{recursive:true});
 const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
 const artifacts={};for(const mode of ['st','mt'])artifacts[mode]=sha(await readFile(join(packages[mode],'z_wasm_bg.wasm')));
 await writeFile(join(output,'report.json'),JSON.stringify({scope:'Public synthetic watch-only Mainnet-profile wallet and actual SDK driver/native PIR crypto; no production consensus chain, controller, service capacity, OHTTP integration or deployment claim',profile:'ironwood-enhance-pir-v7/simplepir-p16-q48-v1',rows:4096,artifacts,reports},null,2)+'\n');
}finally {
 await browser?.close();if(server)await new Promise(resolve=>server.close(resolve));
 if(!childExited){child.kill('SIGTERM');try{await deadline(exited,3000,'fixture stop deadline');}catch{child.kill('SIGKILL');await exited;}}
}
