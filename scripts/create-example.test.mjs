import { test } from 'node:test';
import { markdownAnchors } from './doc-anchors.mjs';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync, existsSync, rmSync, mkdirSync, symlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { copyTemplate, createExample, inspectArchives, archiveMembers, generatedReadme, readPreviewRevision } from './create-example.mjs';

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

test('generated wallet guides retain setup links from the actual templates', () => {
  for (const name of ['browser-wallet', 'next-wallet', 'react-wallet', 'local-passkey']) {
    const source = readFileSync(new URL(`../examples/${name}/README.md`, import.meta.url), 'utf8');
    const generated = generatedReadme(source);
    for (const link of source.matchAll(/https:\/\/github.com\/tzarebczan\/z-stack[^)]+/g)) {
      assert.ok(generated.includes(link[0]), `${name}: missing ${link[0]}`);
    }
    assert.doesNotMatch(generated, /Copy this directory|\/path\/to\//);
  }
});

test('copied wallet setup guard explains missing SDK dependency and accepts manual setup', () => {
  for (const name of ['browser-wallet', 'next-wallet']) {
    const dir = mkdtempSync(join(tmpdir(), 'sdk-template-guard-'));
    try {
      copyTemplate(name, dir);
      const guard = join(dir, 'scripts/check-setup.mjs');
      const missing = spawnSync(process.execPath, [guard], {encoding:'utf8'});
      assert.equal(missing.status, 1);
      assert.match(missing.stderr, /source template.*prebuilt preview/);
      const manifest = JSON.parse(readFileSync(join(dir, 'package.json')));
      manifest.dependencies['@z-stack/sdk'] = 'file:vendor/sdk.tgz';
      writeFileSync(join(dir, 'package.json'), JSON.stringify(manifest));
      assert.equal(spawnSync(process.execPath, [guard], {encoding:'utf8'}).status, 0);
    } finally { rmSync(dir, {recursive:true, force:true}); }
  }
});

test('preview source receipt requires a clean matching SDK archive and valid revision', () => {
  const dir = mkdtempSync(join(tmpdir(), 'sdk-preview-receipt-'));
  const file = join(dir, 'PREVIEW.json');
  const sdk = {version:'0.1.0-test', sha256:'a'.repeat(64)};
  const revision = '1234567890abcdef1234567890abcdef12345678';
  const receipt = {version:sdk.version, revision, dirty:false, packages:{'@z-stack/sdk':sdk}};
  try {
    assert.equal(readPreviewRevision(file, sdk), null);
    writeFileSync(file, JSON.stringify(receipt));
    assert.equal(readPreviewRevision(file, sdk), revision);
    for (const change of [
      {dirty:true}, {revision:'not-a-commit'}, {version:'another-version'},
      {packages:{'@z-stack/sdk':{...sdk, sha256:'b'.repeat(64)}}},
      {packages:{'@z-stack/sdk':{...sdk, version:'another-version'}}},
      {packages:{}},
    ]) {
      writeFileSync(file, JSON.stringify({...receipt, ...change}));
      assert.equal(readPreviewRevision(file, sdk), null, JSON.stringify(change));
    }
  } finally { rmSync(dir, {recursive:true, force:true}); }
});

test('installed generated instructions remove the redundant install command',()=>{
 const input='# Demo\n\nDescription.\n\nInstall:\n\n```sh\nnpm install /path/sdk.tgz\nnpm run dev\n```\n';
 const result=generatedReadme(generatedReadme(input),true);
 assert.doesNotMatch(result,/npm install/);assert.match(result,/Dependencies were installed/);assert.match(result,/npm run dev/);
});
