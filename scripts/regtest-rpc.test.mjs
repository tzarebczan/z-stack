import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
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
