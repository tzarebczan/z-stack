// Disposable public PIR protocol proof. No wallet data, production service or node.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { readFile, mkdir, writeFile } from 'node:fs/promises';
import { resolve, join, sep } from 'node:path';
import { once } from 'node:events';
import { chromium, firefox } from 'playwright';
const root = resolve(import.meta.dirname, '..');
const output = resolve(process.argv[2] ?? '/tmp/coffer-enhance-v7-interop');
const packages={st:resolve(root,process.env.ENHANCE_ST_DIR??'test-output/enhance-v7-st'),mt:resolve(root,process.env.ENHANCE_MT_DIR??'test-output/enhance-v7-mt')};
const binary = process.env.ENHANCE_FIXTURE_BIN ?? '/home/tom/.cache/coffer-enhance-sdk-target/debug/examples/fixture';
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
  const mod = await import('/'+mode+'/z_enhance_pir.js');
  await mod.default();
  if(mode==='mt') await mod.initThreadPool(2);
  const meta = await (await fetch('/metadata')).json();
  const encode = v=>new TextEncoder().encode(JSON.stringify(v));
  const session = mod.EnhanceV7Session.open(encode(meta.manifest),encode(meta.material),BigInt(meta.height),meta.hash,BigInt(meta.records),100n);
  const check=(value,msg)=>{if(!value)throw new Error(msg);};
  const rejects=(fn,code)=>{try{fn();}catch(error){check(String(error).includes(code),'wrong refusal '+String(error));return;}throw new Error('missing refusal '+code);};
  const fresh=session.prepare(0n,100n), another=session.prepare(0n,100n);
  const left=fresh.body().slice(116),right=another.body().slice(116);
  check(left.length===right.length&&left.some((b,i)=>b!==right[i]),'PIR payload reused randomness');
  fresh.cancel();another.cancel();fresh.free();another.free();
  const count=mod.thread_count();check(count===(mode==='mt'?2:1),'unexpected Rayon count');
  let queries=0;
  for(const position of [0,32,33,66]) {
   const query=session.prepare(BigInt(position),100n), body=query.body();
   check(body.length===110708,'request width changed');
   const reply=new Uint8Array(await (await fetch('/query',{method:'POST',body})).arrayBuffer());
   check(reply.length===30836,'response width changed');
   const record=query.finish(reply,100n);
   const hex=Array.from(record,b=>b.toString(16).padStart(2,'0')).join('');
   check(hex===meta.expected[position],'actual PIR record differs');
   rejects(()=>query.finish(reply,100n),'enhance_context_consumed');query.free();queries++;
  }
  const malformed=session.prepare(0n,100n);rejects(()=>malformed.finish(new Uint8Array([1]),100n),'enhance_reply_invalid');rejects(()=>malformed.body(),'enhance_context_consumed');malformed.free();
  const canceled=session.prepare(0n,100n);canceled.cancel();rejects(()=>canceled.body(),'enhance_context_consumed');canceled.free();
  rejects(()=>session.prepare(67n,100n),'enhance_position_invalid');rejects(()=>session.prepare(0n,130n),'enhance_routing_refresh_required');
  rejects(()=>mod.EnhanceV7Session.open(encode(meta.manifest),encode(meta.material),BigInt(meta.height),'00'.repeat(32),BigInt(meta.records),100n),'enhance_anchor_mismatch');
  const corrupt={...meta.material,public_params_base64:'AA=='};
  rejects(()=>mod.EnhanceV7Session.open(encode(meta.manifest),encode(corrupt),BigInt(meta.height),meta.hash,BigInt(meta.records),100n),'enhance_material_invalid');
  session.free();
  self.postMessage({ok:true,mode,threads:count,queries,checks:['real-native-server-records','browser-CSPRNG-fresh-ciphertext-payload','one-use-success','malformed-consumes','cancel-consumes','outside-coverage-refusal','routing-refresh-required','wrong-anchor-refusal','material-digest-refusal']});
 } catch(error) { self.postMessage({ok:false,error:String(error)}); }
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
   if(url.pathname==='/favicon.ico'){res.writeHead(204).end();return;}
   if(url.pathname==='/runner-worker.mjs'){res.writeHead(200,{'Content-Type':'application/javascript'}).end(worker);return;}
   if(url.pathname==='/main.mjs'){res.writeHead(200,{'Content-Type':'application/javascript'}).end(main);return;}
   const match=/^\/(st|mt)\/(.*)$/.exec(url.pathname);
   if(!match){res.writeHead(404).end();return;}
   if(match[2]==='test'){res.writeHead(200,{'Content-Type':'text/html'}).end('<!doctype html><script type="module" src="/main.mjs"></script>');return;}
   const base=packages[match[1]];
   const path=resolve(base,match[2]||'z_enhance_pir.js');assert.ok(path.startsWith(base+sep));
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
    console.log(name+' '+mode+': real v7/q48 query/decode passed, '+result.threads+' thread(s)');
   }finally{await context.close();}
  }
  await browser.close();browser=undefined;
 }
 await mkdir(output,{recursive:true});
 const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
 const artifacts={};for(const mode of ['st','mt'])artifacts[mode]=sha(await readFile(join(packages[mode],'z_enhance_pir_bg.wasm')));
 await writeFile(join(output,'report.json'),JSON.stringify({scope:'Synthetic public protocol-only crypto; no wallet apply, full Valar HTTP service, production capacity or deployment claim',profile:'ironwood-enhance-pir-v7/simplepir-p16-q48-v1',rows:4096,artifacts,reports},null,2)+'\n');
}finally {
 await browser?.close();if(server)await new Promise(resolve=>server.close(resolve));
 if(!childExited){child.kill('SIGTERM');try{await deadline(exited,3000,'fixture stop deadline');}catch{child.kill('SIGKILL');await exited;}}
}
