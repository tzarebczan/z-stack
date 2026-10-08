import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { execFile } from 'node:child_process';
import { mkdtemp, writeFile, access, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { promisify } from 'node:util';
import { regtestRpcUrl, regtestLwdUrl } from './regtest-rpc.mjs';
test('regtest RPC follows the published port and preserves an explicit endpoint',()=>{
 assert.equal(regtestRpcUrl({}),'http://127.0.0.1:29232');
 assert.equal(regtestRpcUrl({Z_STACK_REGTEST_RPC_PORT:'39232'}),'http://127.0.0.1:39232');
 assert.equal(regtestRpcUrl({Z_STACK_REGTEST_RPC_PORT:'39232',Z_STACK_ZEBRA_RPC:'http://127.0.0.1:49232'}),'http://127.0.0.1:49232');
 for(const port of ['0','65536','localhost:123','-1','1.5'])assert.throws(()=>regtestRpcUrl({Z_STACK_REGTEST_RPC_PORT:port}),/must be a port/);
});
test('startup readiness reaches the port override without a second endpoint variable',async()=>{
 const requests=[];
 const server=createServer(async(req,res)=>{let body='';for await(const chunk of req)body+=chunk;requests.push(JSON.parse(body));res.setHeader('content-type','application/json');res.end(JSON.stringify({result:{chain:'regtest',blocks:2}}));});
 await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
 const previousPort=process.env.Z_STACK_REGTEST_RPC_PORT, previousUrl=process.env.Z_STACK_ZEBRA_RPC;
 try {
  process.env.Z_STACK_REGTEST_RPC_PORT=String(server.address().port);delete process.env.Z_STACK_ZEBRA_RPC;
  const module=await import('./regtest-rpc.mjs?port-acceptance');
  assert.deepEqual(await module.waitForZebra(1000),{chain:'regtest',blocks:2});assert.equal(requests[0].method,'getblockchaininfo');
 } finally {
  if(previousPort===undefined)delete process.env.Z_STACK_REGTEST_RPC_PORT;else process.env.Z_STACK_REGTEST_RPC_PORT=previousPort;
  if(previousUrl===undefined)delete process.env.Z_STACK_ZEBRA_RPC;else process.env.Z_STACK_ZEBRA_RPC=previousUrl;
  await new Promise(resolve=>server.close(resolve));
 }
});

test('regtest LWD uses the published port with an explicit URL taking precedence', () => {
 assert.equal(regtestLwdUrl({}), 'http://127.0.0.1:28137');
 assert.equal(regtestLwdUrl({Z_STACK_REGTEST_LWD_PORT:'38137'}),'http://127.0.0.1:38137');
 assert.equal(regtestLwdUrl({Z_STACK_REGTEST_LWD_PORT:'38137',Z_STACK_REGTEST_LWD:'http://localhost:48137'}),'http://localhost:48137');
 for(const port of ['0','65536','host:123','-1','1.5']) assert.throws(()=>regtestLwdUrl({Z_STACK_REGTEST_LWD_PORT:port}),/must be a port/);
});

test('native fixture launcher rejects both directions of a missing or different NU7 schedule before mining or starting Zaino', async () => {
 const scratch=await mkdtemp(join(tmpdir(),'z-stack-native-schedule-'));
 const binary=join(scratch,'fixture-node.mjs');
 await writeFile(binary,'#!/usr/bin/env node\nprocess.exit(0);\n',{mode:0o755});
 let nu7;
 const requests=[];
 const server=createServer(async(req,res)=>{
  let body='';for await(const chunk of req)body+=chunk;
  requests.push(JSON.parse(body).method);
  res.setHeader('content-type','application/json');
  const upgrades={nu63:{name:'NU6.3',activationheight:150}};
  if(nu7!==undefined)upgrades.nu7={name:'NU7',activationheight:nu7};
  res.end(JSON.stringify({result:{chain:'regtest',blocks:2,bestblockhash:'01',upgrades}}));
 });
 await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
 try {
  for(const [i,configured,reported] of [[0,undefined,250],[1,'250',undefined],[2,'250',251]]){
   nu7=reported;requests.length=0;
   const dir=join(scratch,String(i));
   const env={...process.env,Z_STACK_REGTEST_DIR:dir,Z_STACK_REGTEST_NU6_3:'150',
    Z_STACK_ZEBRA_RPC:`http://127.0.0.1:${server.address().port}`,ZAKURAD:binary,ZAINOD:binary};
   if(configured===undefined)delete env.Z_STACK_REGTEST_NU7;else env.Z_STACK_REGTEST_NU7=configured;
   await assert.rejects(promisify(execFile)(process.execPath,['scripts/regtest-native.mjs','up'],{env,timeout:15000}),
    error=>{assert.equal(error.code,1);assert.match(error.stderr,/validator reports NU7.*match the client and validator schedules/);return true;});
   assert.ok(requests.length>0);assert.ok(requests.every(method=>method==='getblockchaininfo'));
   await assert.rejects(access(join(dir,'zaino.pid')),/ENOENT/);
  }
 } finally {await new Promise(resolve=>server.close(resolve));await rm(scratch,{recursive:true,force:true});}
});
