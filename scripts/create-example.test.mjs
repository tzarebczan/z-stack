import { test } from 'node:test';
import { markdownAnchors } from './doc-anchors.mjs';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, existsSync, rmSync, mkdirSync, symlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { copyTemplate, createExample, inspectArchives, archiveMembers, generatedReadme } from './create-example.mjs';

test('scaffolding refuses an occupied destination before reading archives', () => {
  const dir=mkdtempSync(join(tmpdir(),'sdk-scaffold-'));
  try {writeFileSync(join(dir,'keep.txt'),'keep');assert.throws(()=>createExample('browser-wallet',dir,'/missing'),/not empty/);assert.ok(existsSync(join(dir,'keep.txt')));} finally {rmSync(dir,{recursive:true,force:true});}
});
test('scaffolding refuses a destination symlink and incomplete archive sets', () => {
 const dir=mkdtempSync(join(tmpdir(),'sdk-scaffold-'));
 try {mkdirSync(join(dir,'target'));symlinkSync(join(dir,'target'),join(dir,'link'),'dir');assert.throws(()=>createExample('browser-wallet',join(dir,'link'),'/missing'),/ordinary directory/);assert.throws(()=>inspectArchives(dir),/Missing/);} finally {rmSync(dir,{recursive:true,force:true});}
});
test('template copying excludes local files absent from the reviewed manifest',()=>{
 const dir=mkdtempSync(join(tmpdir(),'sdk-scaffold-'));
 // Copying is driven by an explicit file inventory, never by recursive discovery.
 try {copyTemplate('browser-wallet',dir);assert.ok(existsSync(join(dir,'src/send.ts')));assert.equal(existsSync(join(dir,'node_modules')),false);assert.throws(()=>copyTemplate('../browser-wallet',dir),/Unknown template/);} finally {rmSync(dir,{recursive:true,force:true});}
});
test('archive inspection rejects invalid compression',()=>{
 const dir=mkdtempSync(join(tmpdir(),'sdk-scaffold-'));
 try {const file=join(dir,'invalid.tgz');writeFileSync(file,'not an archive');assert.throws(()=>archiveMembers(file));} finally {rmSync(dir,{recursive:true,force:true});}
});

test('Base addon is explicit and refuses unsupported templates', () => {
  assert.throws(() => createExample('local-passkey', '/unused', '/missing', {withBase:true}), /supports browser-wallet/);
});


test('generated instructions use already-vendored archives and retain template commands', () => {
  const input = '# Demo\n\nDescription.\n\nCopy this directory and install archives:\n\n```sh\ncd /path/to/copied/demo\nnpm install /path/core.tgz \\\n  /path/passkey.tgz \\\n  /path/sdk.tgz\nnpm run build\nnpm run start\n```\n\nMore guidance.\n';
  const result = generatedReadme(input);
  assert.doesNotMatch(result, /Copy this directory|\/path\//);
  assert.match(result, /npm install --ignore-scripts/);
  assert.match(result, /npm run build\nnpm run start/);
  assert.match(result, /Description/);
  assert.match(result, /More guidance/);
});


test('heading anchors reject stale quickstart links and ignore fenced headings', () => {
  const ids = markdownAnchors('# SDK\n## Build from source\n## Build from source\n```sh\n# build-preview-packages\n```\n## `@z-stack/sdk` — API\n');
  assert.ok(ids.has('build-from-source'));
  assert.ok(ids.has('build-from-source-1'));
  assert.ok(ids.has('z-stacksdk--api'));
  assert.ok(!ids.has('build-preview-packages'));
});
