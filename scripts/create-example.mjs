#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { gunzipSync } from 'node:zlib';
import { cpSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, renameSync, rmSync, rmdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const templateFiles = JSON.parse(readFileSync(join(root, 'examples/templates.json')));
const templates = Object.keys(templateFiles);
const checksum = bytes => createHash('sha256').update(bytes).digest('hex');

// Read known members without extracting paths, following links or running code.
export function archiveMembers(file) {
  const data = gunzipSync(readFileSync(file), { maxOutputLength: 128 * 1024 * 1024 });
  const members = new Map();
  for (let offset = 0; offset + 512 <= data.length;) {
    const header = data.subarray(offset, offset + 512);
    if (header.every(byte => byte === 0)) break;
    const text = (start, size) => header.subarray(start, start + size).toString('utf8').replace(/\0.*$/s, '');
    const sizeText = text(124, 12).trim();
    assert.ok(/^[0-7]+$/.test(sizeText), 'Invalid tar member size');
    const size = parseInt(sizeText, 8);
    const name = [text(345, 155), text(0, 100)].filter(Boolean).join('/');
    assert.ok(Number.isSafeInteger(size) && offset + 512 + size <= data.length, 'Truncated archive');
    const type = text(156, 1);
    if (type === '' || type === '0') {
      assert.ok(!members.has(name), `Duplicate archive member: ${name}`);
      members.set(name, data.subarray(offset + 512, offset + 512 + size));
    }
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  return members;
}

export function inspectArchives(directory, version = JSON.parse(readFileSync(join(root, "package.json"))).version, names = ["core", "passkey", "sdk"]) {
  const candidates = names.map(name => `z-stack-${name}-${version}.tgz`);
  for (const name of candidates) assert.ok(existsSync(join(directory, name)), `Missing ${name}; use the extracted published preview bundle, or run pnpm build:sdk then pnpm pack:sdk in the source checkout (pnpm pack:base for Base)`);
  const packages = new Map();
  for (const filename of candidates) {
    const file = join(directory, filename);
    assert.ok(lstatSync(file).isFile() && !lstatSync(file).isSymbolicLink(), 'Archives must be regular files');
    const members = archiveMembers(file);
    const manifest = JSON.parse(members.get('package/package.json')?.toString() || 'null');
    assert.ok(manifest && /^@z-stack\/(core|passkey|sdk|base)$/.test(manifest.name), 'Unexpected archive package');
    assert.equal(manifest.name, `@z-stack/${names[candidates.indexOf(filename)]}`, "Archive filename does not match its package");
    assert.equal(manifest.version, version, 'Archive manifest version differs from the selected version');
    assert.ok(!packages.has(manifest.name), `Multiple versions of ${manifest.name}: choose a directory with one matching set`);
    for (const legal of ['LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.txt', 'THIRD_PARTY_DEPENDENCIES.json']) {
      assert.ok(members.has('package/' + legal), `${manifest.name}: missing ${legal}`);
    }
    assert.ok(members.has('package/dist/index.js'), `${manifest.name}: missing built entry point`);
    if (manifest.name === '@z-stack/sdk') {
      for (const dep of ['core', 'passkey']) {
        const helper = packages.get(`@z-stack/${dep}`);
        assert.ok(helper, `Missing matching ${dep} archive`);
        for (const [name, bytes] of archiveMembers(helper.file)) {
          const bundled = members.get(name.replace('package/', `package/node_modules/@z-stack/${dep}/`));
          assert.ok(bundled?.equals(bytes), `SDK bundled ${dep} differs from the matching archive; run pnpm pack:sdk`);
        }
      }
      for (const kind of ['generated', 'generated-mt']) {
        const bytes = members.get(`package/dist/${kind}/z_wasm_bg.wasm`);
        const integrity = JSON.parse(members.get(`package/dist/${kind}/integrity.json`)?.toString() || 'null');
        assert.ok(bytes?.length > 1_000_000 && bytes.subarray(0, 4).equals(Buffer.from([0, 97, 115, 109])), 'Missing production WASM');
        assert.equal(checksum(bytes), integrity?.sha256, `${kind}: WASM integrity mismatch`);
      }
    }
    packages.set(manifest.name, { filename, file, version: manifest.version, sha256: checksum(readFileSync(file)) });
  }
  assert.equal(packages.size, names.length, "Expected one archive per selected package");
  assert.equal(new Set([...packages.values()].map(pkg => pkg.version)).size, 1, 'Archive versions must match');
  return packages;
}

// Copy only reviewed template files; local credentials and scratch files stay behind.
export function copyTemplate(template, destination) {
  assert.ok(templates.includes(template), 'Unknown template');
  const source = join(root, 'examples', template);
  assert.ok(lstatSync(source).isDirectory() && !lstatSync(source).isSymbolicLink(), 'Template must be an ordinary directory');
  for (const relative of templateFiles[template]) {
    assert.ok(typeof relative === 'string' && /^[a-zA-Z0-9_.\/-]+$/.test(relative) && !relative.startsWith('/') && !relative.split('/').some(part => part === '..' || part === '.'), 'Invalid template path');
    let file = source;
    for (const part of relative.split('/')) {
      file = join(file, part);
      assert.ok(!lstatSync(file).isSymbolicLink(), 'Template paths must not contain symlinks');
    }
    assert.ok(lstatSync(file).isFile(), 'Template entry must be a regular file');
    const target = join(destination, relative);
    mkdirSync(dirname(target), { recursive: true });
    cpSync(file, target);
  }
}

export function generatedReadme(text, installed = false) {
  const match = /```sh\n([\s\S]*?)\n```/.exec(text);
  if (!match) return text.replace(/From the repository,[\s\S]*?In the generated directory, run `npm run dev`\./,
    installed ? 'Dependencies are installed. Run `npm run dev` in this directory.' : 'Matching archives are already in `vendor/`. Run `npm install --ignore-scripts`, then `npm run dev` in this directory.');
  const start = text.slice(0, match.index).trimEnd().lastIndexOf('\n\n');
  const commands = match[1].replace(/(?:cd [^\n]+\n)?npm install [^\n]*(?:\n +[^\n]*)*/,
    installed ? '' : 'npm install --ignore-scripts').trim();
  return text.slice(0, start < 0 ? match.index : start) +
    '\n\nMatching SDK archives are already in `vendor/`; package.json uses local file dependencies. ' +
    (installed ? 'Dependencies were installed by `--install`. Run from this app directory:' : 'Install the dependencies once, then run from this app directory:') + '\n\n```sh\n' +
    commands + '\n```' + text.slice(match.index + match[0].length);
}

export function readPreviewRevision(previewFile, sdk) {
  if (!sdk || !existsSync(previewFile)) return null;
  const preview = JSON.parse(readFileSync(previewFile, 'utf8'));
  const entry = preview?.packages?.['@z-stack/sdk'];
  return preview?.dirty === false && preview.version === sdk.version &&
    /^[0-9a-f]{40}$/.test(preview.revision) && entry?.version === sdk.version &&
    entry?.sha256 === sdk.sha256 ? preview.revision : null;
}

export function createExample(template, destination, archives, { withBase = false } = {}) {
  assert.ok(!withBase || ["browser-wallet", "next-wallet"].includes(template), "--with-base supports browser-wallet and next-wallet");
  assert.ok(templates.includes(template), `Choose ${templates.join(', ')}`);
  const target = resolve(destination);
  if (existsSync(target)) {
    assert.ok(lstatSync(target).isDirectory() && !lstatSync(target).isSymbolicLink(), 'Destination must be an ordinary directory');
    assert.equal(readdirSync(target).length, 0, 'Destination is not empty; choose a new subdirectory or an empty directory. Existing files will not be overwritten');
  }
  const packages = inspectArchives(resolve(archives), undefined, template === "base-wallet" ? ["base"] : ["core", "passkey", "sdk", ...(withBase ? ["base"] : [])]);
  mkdirSync(dirname(target), { recursive: true });
  const scratch = mkdtempSync(join(dirname(target), '.z-stack-example-'));
  try {
    copyTemplate(template, scratch);
    mkdirSync(join(scratch, 'vendor'));
    if (template !== 'base-wallet') {
      mkdirSync(join(scratch, 'scripts'), { recursive: true });
      const checker = join(root, 'scripts/check-testnet-chain.mjs');
      assert.ok(lstatSync(checker).isFile() && !lstatSync(checker).isSymbolicLink(), 'Chain checker must be a regular file');
      cpSync(checker, join(scratch, 'scripts/check-testnet-chain.mjs'));
    }
    const manifestFile = join(scratch, 'package.json');
    const manifest = JSON.parse(readFileSync(manifestFile));
    if (template !== 'base-wallet') manifest.scripts['check:chain'] = 'node scripts/check-testnet-chain.mjs';
    for (const [name, pkg] of packages) {
      cpSync(pkg.file, join(scratch, 'vendor', pkg.filename));
      // The SDK archive includes its core/passkey helpers. Installing those again
      // at the app root creates duplicate class identities; use SDK re-exports.
      if (name === '@z-stack/sdk' || name === '@z-stack/base') {
        (manifest.dependencies ??= {})[name] = `file:vendor/${pkg.filename}`;
      }
    }
    if (withBase) {
      const addon = join(root, 'examples', 'shared-base');
      const files = template === 'browser-wallet'
        ? [['base-panel.ts', 'src/base-panel.ts'], ['vite-adapter.ts', 'src/base.ts']]
        : [['base-panel.ts', 'lib/base-panel.ts'], ['next-component.tsx', 'components/BaseWallet.tsx']];
      assert.ok(!lstatSync(addon).isSymbolicLink(), 'Base addon must not be a symlink');
      for (const [source, target] of files) {
        const file = join(addon, source);
        assert.ok(lstatSync(file).isFile() && !lstatSync(file).isSymbolicLink(), 'Base addon entries must be ordinary files');
        cpSync(file, join(scratch, target));
      }
      const baseManifest = JSON.parse(archiveMembers(packages.get('@z-stack/base').file).get('package/package.json'));
      manifest.dependencies.viem = baseManifest.dependencies.viem;
      const css = template === 'browser-wallet' ? 'src/style.css' : 'app/globals.css';
      writeFileSync(join(scratch, css), readFileSync(join(scratch, css), 'utf8') + '\n.base-panel { margin-block: 32px; padding-block: 24px; border-block: 1px solid #829b90; min-width: 0; }\n.base-panel code, .base-panel [role=status], #base-review { overflow-wrap: anywhere; }\n.base-panel select { font: inherit; width: 100%; min-height: 48px; border: 1px solid #829b90; border-radius: 8px; padding: 10px; background: white; }\n.base-panel [hidden] { display: none !important; }\n');
      writeFileSync(join(scratch, 'README.md'), readFileSync(join(scratch, 'README.md'), 'utf8') + '\n## Optional Base wallet\n\nThis app was scaffolded with `--with-base`. Restore or create the Zcash wallet first, then select **Enable Base Sepolia**. Use that same disposable recovery phrase. Each Base payment verifies the phrase against the selected Zcash wallet and clears it after unlocking. No separate seed or account service is required.\n\nThe Base panel uses your chosen Base Sepolia RPC, native ETH/USDC payment review, an app-owned Web Lock and a durable pending transaction hash. Check the saved payment before another send, including after a lost acknowledgement or reload. This local journal coordinates this origin only, not other devices. Base transactions and your Base address are public; the RPC sees balance queries and your IP. Base Sepolia is separate from the Zcash network. No paymaster is enabled in this demo.\n');
    }
    writeFileSync(join(scratch, 'README.md'), generatedReadme(readFileSync(join(scratch, 'README.md'), 'utf8')));
    writeFileSync(manifestFile, JSON.stringify(manifest, null, 2) + '\n');
    if (['browser-wallet', 'next-wallet'].includes(template)) {
      const sdk = packages.get('@z-stack/sdk');
      const revision = readPreviewRevision(join(root, 'PREVIEW.json'), sdk);
      writeFileSync(join(scratch, 'sdk-build.json'), JSON.stringify({version: sdk.version, revision, sdkSha256: sdk.sha256}, null, 2) + '\n');
    }
    writeFileSync(join(scratch, 'SDK-ARCHIVES.json'), JSON.stringify(Object.fromEntries([...packages].map(([name, { filename, version, sha256 }]) => [name, { filename, version, sha256 }])), null, 2) + '\n');
    if (existsSync(target)) rmdirSync(target); // Only the verified empty directory.
    renameSync(scratch, target);
  } catch (error) { rmSync(scratch, { recursive: true, force: true }); throw error; }
  return target;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const [template, destination, ...flags] = process.argv.slice(2);
    assert.ok(template && destination, 'Usage: node scripts/create-example.mjs <template> <destination> [--archives <directory>] [--with-base] [--install]');
    let archives = join(root, 'artifacts'), install = false, withBase = false;
    for (let i = 0; i < flags.length; i++) {
      if (flags[i] === '--archives') { assert.ok(flags[i + 1], '--archives needs a directory'); archives = flags[++i]; }
      else if (flags[i] === '--with-base') withBase = true;
      else if (flags[i] === '--install') install = true;
      else throw new Error('Unknown argument: ' + flags[i]);
    }
    const target = createExample(template, destination, archives, { withBase });
    console.log(`Created ${template}: ${target}`);
    if (install) {
      const result = spawnSync('npm', ['install', '--ignore-scripts', '--no-audit', '--no-fund'], {
        cwd: target, stdio: 'inherit', shell: process.platform === 'win32' });
      assert.equal(result.status, 0, 'Install failed; the generated app is preserved. Retry npm install there.');
      const readme = join(target, 'README.md');
      writeFileSync(readme, generatedReadme(readFileSync(readme, 'utf8'), true));
    }
    console.log(`Next: open that directory, ${install ? 'run npm run dev' : 'run npm install --ignore-scripts, then npm run dev'}. Use test funds only.`);
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
