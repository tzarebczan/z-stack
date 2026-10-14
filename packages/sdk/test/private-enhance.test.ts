import test from "node:test";
import assert from "node:assert/strict";
import { syncPrivateEnhance, type PrivateEnhanceHost, type PrivateEnhanceTransport, type PrivateEnhanceOperation } from "../src/private-enhance";
const encode = (value: unknown) => new TextEncoder().encode(JSON.stringify(value));
const id = "ab".repeat(32);
const anchor = { height: 3500000, hash: "ef".repeat(32) };
function fixture() {
  const operations: PrivateEnhanceOperation[] = [];
  const requests: string[] = [];
  let prepared = 0, checkpoints = 0, active = true;
  const source: PrivateEnhanceHost = { privateEnhance: async (op) => {
    operations.push(op);
    if (op === "pending") return Uint8Array.of(1);
    if (op === "begin") return encode([id]);
    if (op === "anchor") return encode(anchor);
    if (op === "prepare") return prepared++ === 0 ? new Uint8Array(110708) : new Uint8Array();
    if (op === "finish") return new TextEncoder().encode("Stored");
    return new Uint8Array();
  } };
  const transport: PrivateEnhanceTransport = { exchange: async (request) => {
    requests.push(request.kind);
    if (request.kind === "session") assert.equal(request.sessionId,id);
    if (request.kind === "query") { assert.deepEqual(request.anchor,anchor); assert.equal(request.body.length,110708); }
    return { status: 200, body: new Uint8Array(request.kind === "query" ? 30836 : 1) };
  } };
  return {source,transport,operations,requests,signal:new AbortController().signal,
    assertCurrent:()=>{assert.equal(active,true,"retired owner");},checkpoint:async()=>{checkpoints++;},
    checkpoints:()=>checkpoints,retire:()=>{active=false;}};
}
test("closed Enhancement pass sends public session ids and ciphertext, then persists Rust apply",async()=>{
  const f=fixture();assert.equal(await syncPrivateEnhance(f),1);
  assert.deepEqual(f.requests,["init","session","query"]);
  assert.deepEqual(f.operations,["pending","begin","material","anchor","prepare","finish","prepare","cancel"]);
  assert.equal(f.checkpoints(),1);
});
test("no queued action produces zero provider calls",async()=>{
  const f=fixture();f.source.privateEnhance=async op=>{f.operations.push(op);return Uint8Array.of(0);};
  assert.equal(await syncPrivateEnhance(f),0);assert.deepEqual(f.requests,[]);assert.deepEqual(f.operations,["pending","cancel"]);
});
test("provider refresh and malformed size retire Rust contexts without apply",async()=>{
  for(const status of [409,410,429,503] as const){
    const f=fixture();f.transport.exchange=async()=>({status,body:new Uint8Array()});
    await assert.rejects(syncPrivateEnhance(f),/enhance_(refresh_required|unavailable)/);
    assert.deepEqual(f.operations,["pending","cancel"]);assert.equal(f.checkpoints(),0);
  }
  const f=fixture();f.transport.exchange=async()=>({status:200,body:new Uint8Array(1024*1024+1)});
  await assert.rejects(syncPrivateEnhance(f),/enhance_size_limit/);assert.equal(f.operations.at(-1),"cancel");
});
test("owner retirement after delayed reply never applies and always cancels",async()=>{
  const f=fixture();const real=f.transport.exchange;
  f.transport.exchange=async(request,signal)=>{const result=await real(request,signal);if(request.kind==="query")f.retire();return result;};
  await assert.rejects(syncPrivateEnhance(f),/retired owner/);
  assert.ok(!f.operations.includes("finish"));assert.equal(f.operations.at(-1),"cancel");assert.equal(f.checkpoints(),0);
});
test("abort stops a hanging provider promptly and consumes before a late resolution",async()=>{
  const f=fixture();const controller=new AbortController();let late:((value:{status:200;body:Uint8Array})=>void)|undefined;
  f.transport.exchange=async()=>new Promise(resolve=>{late=resolve;});
  const running=syncPrivateEnhance({...f,signal:controller.signal});
  await new Promise(resolve=>setImmediate(resolve));controller.abort(new Error("owned cancel"));
  await assert.rejects(running,/owned cancel/);late?.({status:200,body:new Uint8Array(1)});
  await new Promise(resolve=>setImmediate(resolve));assert.deepEqual(f.operations,["pending","cancel"]);
});
