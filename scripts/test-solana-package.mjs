#!/usr/bin/env node
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, existsSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { launchBrowser } from './browser-launch.mjs';
import { inspectArchives } from './create-example.mjs';
import { reuseConsumerArchives } from './consumer-archives.mjs';
const root = join(dirname(fileURLToPath(import.meta.url)), '..'), scratch = mkdtempSync(join(tmpdir(), 'z-stack-solana-consumer-'));
const archives = join(scratch, 'archives'), app = join(scratch, 'app');
const run = (command, args, cwd = root) => { const result = spawnSync(command, args, { cwd, stdio: 'inherit' }); assert.equal(result.status, 0, `${command} failed`); };
try {
  if (!reuseConsumerArchives(root, archives, ['solana', 'swaps'])) run(process.execPath, ['scripts/pack-sdk.mjs', archives, '--swaps-only']);
  inspectArchives(archives, undefined, ['solana', 'swaps']);
  mkdirSync(app);
  writeFileSync(join(app, 'package.json'), JSON.stringify({ private: true, type: 'module', devDependencies: { esbuild: '0.25.12' } }));
  const version = JSON.parse(readFileSync(join(root, 'package.json'))).version;
  run('npm', ['install', '--ignore-scripts', '--no-audit', '--no-fund', ...['solana', 'swaps'].map(name => join(archives, `z-stack-${name}-${version}.tgz`))], app);
  for (const name of ['solana', 'swaps']) {
    const dir = join(app, 'node_modules/@z-stack', name), pkg = JSON.parse(readFileSync(join(dir, 'package.json')));
    assert.ok(!existsSync(join(dir, 'src')), 'Repository sources must not ship');
    for (const entry of Object.values(pkg.exports)) {
      assert.ok(!('@z-stack/source' in entry));
      for (const file of Object.values(entry)) assert.ok(existsSync(join(dir, file)));
    }
    for (const value of Object.values(pkg.dependencies ?? {})) assert.ok(!/^(workspace:|file:|link:)/.test(value));
    for (const file of ['LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.txt', 'THIRD_PARTY_DEPENDENCIES.json']) assert.ok(existsSync(join(dir, file)));
  }
  for (const name of ['sdk', 'core', 'passkey']) assert.ok(!existsSync(join(app, 'node_modules/@z-stack', name)), `${name} must stay optional`);
  writeFileSync(join(app, 'consumer.mjs'), `
    import assert from 'node:assert/strict';
    import {deriveSolanaAddress,SOLANA_CYPH,PreparedSolanaSwapSchema,createSolanaRpcClient,getSolanaRingAssetSupport,verifySolanaRingDeposit} from '@z-stack/solana';
    import {createTwoLegController,createMemoryTwoLegRouteStore,TwoLegRouteSchema,getAuroraRingAssetSupport} from '@z-stack/swaps';
    assert.equal(deriveSolanaAddress('abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about'),'HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk');
    assert.equal(SOLANA_CYPH.decimals,6); assert.equal(typeof createSolanaRpcClient,'function');
    assert.equal(PreparedSolanaSwapSchema.safeParse({}).success,false);
    assert.equal(typeof createTwoLegController,'function'); assert.equal(typeof createMemoryTwoLegRouteStore,'function');
    assert.equal(TwoLegRouteSchema.safeParse({}).success,false);
    assert.equal(typeof getSolanaRingAssetSupport,'function');
    assert.equal(typeof verifySolanaRingDeposit,'function');
    assert.equal(typeof getAuroraRingAssetSupport,'function');
  `);
  run(process.execPath, ['consumer.mjs'], app);
  writeFileSync(join(app, 'browser.mjs'), `import {deriveSolanaAddress} from '@z-stack/solana';import {createMemoryTwoLegRouteStore} from '@z-stack/swaps';globalThis.solanaConsumer={deriveSolanaAddress,createMemoryTwoLegRouteStore};`);
  writeFileSync(join(app, 'build.mjs'), `import {build} from 'esbuild';await build({entryPoints:['browser.mjs'],bundle:true,format:'esm',platform:'browser',outfile:'dist.js'});`);
  run(process.execPath, ['build.mjs'], app);
  assert.ok(readFileSync(join(app, 'dist.js')).length > 1000);
  if (process.argv.includes('--browser')) {
    const playwright = createRequire(join(root, 'package.json'))('playwright');
    const names = (process.argv.find(value => value.startsWith('--browsers='))?.slice(11) ?? 'chromium').split(',');
    assert.ok(names.every(name => ['chromium', 'firefox', 'webkit'].includes(name)));
    for (const name of names) {
      const browser = await launchBrowser(playwright[name]);
      try {
        const page = await browser.newPage();
        await page.addScriptTag({ type: 'module', content: readFileSync(join(app, 'dist.js'), 'utf8') });
        await page.waitForFunction(() => globalThis.solanaConsumer);
        const address = await page.evaluate(() => globalThis.solanaConsumer.deriveSolanaAddress('abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about'));
        assert.equal(address, 'HAgk14JpMQLgt6rVgv7cBQFJWFto5Dqxi472uT3DKpqk');
        console.log(`Installed Solana/swaps ${name}: real browser derivation passed`);
      } finally { await browser.close(); }
    }
  }
  console.log('Installed Solana/swaps archives: portable imports, no Zcash dependency, real browser bundle passed');
} finally { rmSync(scratch, { recursive: true, force: true }); }
