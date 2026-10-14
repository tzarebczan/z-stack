// Reverse the documented wrapper-only patch and compare every original source digest.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, cp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { execFileSync } from 'node:child_process';
const root=resolve(import.meta.dirname,'..');
const vendor=join(root,'vendor/pir');
const record=JSON.parse(await readFile(join(vendor,'UPSTREAM.json'),'utf8'));
const scratch=await mkdtemp(join(tmpdir(),'enhance-upstream-'));
try {
 for(const name of ['enhance-pir','pir-native'])await cp(join(vendor,name),join(scratch,name),{recursive:true});
 execFileSync('git',['apply','--reverse','--check',join(vendor,'http-client.patch')],{cwd:scratch,stdio:'pipe'});
 execFileSync('git',['apply','--reverse',join(vendor,'http-client.patch')],{cwd:scratch,stdio:'pipe'});
 let checked=0;
 for(const name of ['enhance-pir','pir-native'])for(const [path,expected]of Object.entries(record[name].originalFileHashes)) {
  const actual=createHash('sha256').update(await readFile(join(scratch,name,path))).digest('hex');
  assert.equal(actual,expected,'upstream source differs: '+name+'/'+path);checked++;
 }
 console.log('Verified '+checked+' original upstream file hashes after reversing the documented HTTP/path patch; no PIR math changes.');
}finally{await rm(scratch,{recursive:true,force:true});}
