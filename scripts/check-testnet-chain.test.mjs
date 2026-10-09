import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync,mkdirSync,readFileSync,writeFileSync,rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { pathToFileURL } from 'node:url';
const hash='ab'.repeat(32);
function check(args, { state={height:4465026,hash,network:'test'}, explorerHash=hash, status=200 }={}) {
 const root=mkdtempSync(join(tmpdir(),'chain-check-'));
 try {
  mkdirSync(join(root,'scripts'));mkdirSync(join(root,'packages/sdk/dist'),{recursive:true});
  writeFileSync(join(root,'package.json'),' {"type":"module"}');
  writeFileSync(join(root,'scripts/check-testnet-chain.mjs'),readFileSync(new URL('./check-testnet-chain.mjs',import.meta.url)));
  writeFileSync(join(root,'packages/sdk/dist/index.js'),`export const grpcWebTransport=()=>({treeState:async()=>(${JSON.stringify(state)})});`);
  const preload=join(root,'preload.mjs');
  writeFileSync(preload,`globalThis.fetch=async()=>new Response(JSON.stringify({data:{summary:{block_height:4465026,block_hash:${JSON.stringify(explorerHash)},is_canonical:true}},freshness:{network:'testnet'}}),{status:${status}});`);
  return spawnSync(process.execPath,['--import',pathToFileURL(preload).href,join(root,'scripts/check-testnet-chain.mjs'),...args],{encoding:'utf8'});
 } finally {rmSync(root,{recursive:true,force:true});}
}
test('expected public block match does not depend on explorer availability',()=>{
 const result=check(['--height','4465026','--expected-hash',hash],{status:404});assert.equal(result.status,0,result.stderr);assert.match(result.stdout,/"match": true/);assert.match(result.stdout,/not faucet funding/);
});
test('expected hash mismatch and wrong network fail separately',()=>{
 assert.equal(check(['--height','4465026','--expected-hash','cd'.repeat(32)]).status,1);
 assert.equal(check(['--height','4465026','--expected-hash',hash],{state:{height:4465026,hash,network:'main'}}).status,2);
});
test('invalid hash or credential-bearing endpoint never reports a successful check',()=>{
 for(const args of [['--expected-hash','bad'],['--server','https://user:PRIVATE_FIXTURE@example.com']]) {
  const result=check(['--height','4465026',...args]);assert.equal(result.status,2);assert.doesNotMatch(result.stdout+result.stderr,/PRIVATE_FIXTURE/);
 }
});
test('explorer mismatch is reported as provider disagreement, not spending failure',()=>{
 const result=check(['--height','4465026','--compare-explorer'],{explorerHash:'cd'.repeat(32)});assert.equal(result.status,1);assert.match(result.stderr,/does not by itself prove/);
});
test('missing explorer block is not diagnosed as a fork',()=>{
 const result=check(['--height','4465026','--compare-explorer'],{status:404});assert.equal(result.status,2);assert.match(result.stderr,/missing block alone does not prove a fork/);
});
test('reversed provider encoding is not diagnosed as a genuine mismatch',()=>{
 const ordered=Array.from({length:32},(_,i)=>i.toString(16).padStart(2,'0')).join('');
 const result=check(['--height','4465026','--compare-explorer'],{state:{height:4465026,hash:ordered,network:'test'},explorerHash:ordered.match(/../g).reverse().join('')});assert.equal(result.status,2);assert.match(result.stderr,/reversed byte order/);
});

test('default checks the published NU7 anchor without querying the disagreeing explorer',()=>{
 const canonical='000089ba27100beede16d64b34e3d1b626b428cb7ee9fe6dcfdc217ce24e78af';
 const result=check([],{state:{height:4465026,hash:canonical,network:'test'},status:404});assert.equal(result.status,0,result.stderr);
});
test('custom heights and incompatible check modes require explicit intent',()=>{
 for(const args of [['--height','4465027'],['--compare-explorer','--expected-hash',hash]]) {
  assert.equal(check(args).status,2);
 }
});
