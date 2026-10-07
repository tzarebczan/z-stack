#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync, execFileSync } from 'node:child_process';
import { createExample, inspectArchives } from './create-example.mjs';

const root=resolve(import.meta.dirname,'..');
const args=process.argv.slice(2),outputArgument=args.find(arg=>!arg.startsWith('--'));
assert.ok(args.every(arg=>arg===outputArgument||arg==='--with-base'),'Usage: build-staging-examples.mjs [output-directory] [--with-base]');
const output=resolve(outputArgument||mkdtempSync(join(tmpdir(),'z-stack-staging-')));
mkdirSync(output,{recursive:true});
for(const name of ['vite','next'])assert.ok(!existsSync(join(output,name)), 'Output target already exists');
const scratch=mkdtempSync(join(tmpdir(),'z-stack-staging-consumers-'));
const withBase=process.argv.includes('--with-base');
const archives=inspectArchives(join(root,'artifacts'),undefined,['core','passkey','sdk',...(withBase?['base']:[])]);
const revision=execFileSync('git',['rev-parse','HEAD'],{cwd:root,encoding:'utf8'}).trim();
assert.equal(execFileSync('git',['status','--porcelain','--','.',':(exclude)coord'],{cwd:root,encoding:'utf8'}).trim(),'','Commit SDK source before building staging provenance');
const env={...process.env,NEXT_TELEMETRY_DISABLED:'1',VITE_ZSTACK_SERVER:'https://zcash-testnet.chainsafe.dev',
 NEXT_PUBLIC_ZSTACK_SERVER:'https://zcash-testnet.chainsafe.dev',Z_STACK_STATIC_EXPORT:'1'};
for(const [name,template,dist]of [['vite','browser-wallet','dist'],['next','next-wallet','out']]){
 const app=join(scratch,name);createExample(template,app,join(root,'artifacts'), {withBase});
 for(const args of [['install','--ignore-scripts','--no-audit','--no-fund'],['run','build']]){
  const result=spawnSync('npm',args,{cwd:app,env,stdio:'inherit',shell:process.platform==='win32'});assert.equal(result.status,0,'Staging example build failed');
 }
 const target=join(output,name);cpSync(join(app,dist),target,{recursive:true});
 const hashes=new Set();
 function htmlHashes(dir){for(const entry of readdirSync(dir,{withFileTypes:true})){
  const file=join(dir,entry.name);if(entry.isDirectory())htmlHashes(file);
  else if(entry.name.endsWith('.html'))for(const script of readFileSync(file,'utf8').matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/g)){
   if(!/\bsrc\s*=/.test(script[1])&&script[2])hashes.add(`'sha256-${createHash('sha256').update(script[2]).digest('base64')}'`);
  }
 }}
 htmlHashes(target);
 writeFileSync(join(target,'_headers'),`/*\n  Cross-Origin-Opener-Policy: same-origin\n  Cross-Origin-Embedder-Policy: require-corp\n  X-Content-Type-Options: nosniff\n  Referrer-Policy: no-referrer\n  X-Frame-Options: DENY\n  Content-Security-Policy: default-src 'self'; script-src 'self' 'wasm-unsafe-eval' ${[...hashes].join(' ')}; worker-src 'self' blob:; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self' https://zcash-testnet.chainsafe.dev https://sepolia.base.org; frame-ancestors 'none'; base-uri 'none'; object-src 'none'; form-action 'none'\n`);
 writeFileSync(join(target,'build-info.json'),JSON.stringify({sdkRevision:revision,network:'testnet',template,
  withBase,baseNetwork:withBase?'Base Sepolia':null,sdkVersion:archives.get('@z-stack/sdk').version,archiveSha256:archives.get('@z-stack/sdk').sha256,
  archives:Object.fromEntries([...archives].map(([name,value])=>[name,{version:value.version,sha256:value.sha256}]))},null,2)+'\n');
}
console.log(`Testnet-only staging assets: ${output}`);
console.log(`Consumer source/apps retained for verification: ${scratch}`);
