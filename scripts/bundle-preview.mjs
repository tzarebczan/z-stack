#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync, spawnSync } from 'node:child_process';
import { assertReleaseVersions } from './release-versions.mjs';
import { inspectArchives, copyTemplate } from './create-example.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const version = assertReleaseVersions(root);
const revision = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim();
const changes = execFileSync('git', ['status', '--porcelain', '--untracked-files=normal'], { cwd: root, encoding: 'utf8' })
  .trimEnd().split('\n').filter(line => line && !line.slice(3).startsWith('coord/'));
assert.ok(changes.length === 0 || process.argv.includes('--allow-dirty'), 'Commit the SDK changes before bundling, or use --allow-dirty for a local rehearsal');
// Ignored dist/ and archives can come from an older same-version commit.
// Build from the current sources, then repack before associating them with HEAD.
// The pack gate checks both WASM source fingerprints; stale Rust builds fail.
for (const command of ['build:packages', 'docs:api', 'pack:sdk', 'pack:base']) {
  const result = spawnSync('pnpm', [command], {cwd: root, stdio: 'inherit', shell: process.platform === 'win32'});
  assert.equal(result.status, 0, `${command} failed; no preview source receipt was issued`);
}
assert.equal(execFileSync('git', ['rev-parse', 'HEAD'], {cwd: root, encoding:'utf8'}).trim(), revision,
  'Source revision changed while rebuilding preview artifacts');
const currentChanges = execFileSync('git', ['status', '--porcelain', '--untracked-files=normal'], {cwd: root, encoding:'utf8'})
  .trimEnd().split('\n').filter(line => line && !line.slice(3).startsWith('coord/'));
assert.deepEqual(currentChanges, changes, 'Source worktree changed while rebuilding preview artifacts');
assert.ok(existsSync(join(root, 'docs/api/index.html')), 'Missing rebuilt offline API reference');
const packages = inspectArchives(join(root, 'artifacts'), undefined, ['core', 'passkey', 'sdk', 'base']);
assert.ok([...packages.values()].every(pkg => pkg.version === version), 'Archive versions must match the bundle release version');
const output = join(root, 'artifacts', `z-stack-preview-${version}${changes.length ? '-dirty' : ''}.tgz`);
const scratch = mkdtempSync(join(tmpdir(), 'z-stack-preview-'));
try {
  const bundle = join(scratch, 'z-stack-preview'); mkdirSync(bundle);
  const copy = (source, target = source) => cpSync(join(root, source), join(bundle, target), { recursive: true,
    filter: file => {
      const name = file.split(/[\\/]/).at(-1);
      return !['node_modules', '.next', 'dist', 'out', '.git', 'package-lock.json', 'pnpm-lock.yaml'].includes(name) && !name.startsWith('.env');
    } });
  for (const file of ['LICENSE', 'NOTICE', 'THIRD_PARTY_LICENSES.txt', 'SECURITY.md', 'CHANGELOG.md', 'docs', 'licenses']) copy(file);
  copy('examples/shared-base');
  copy('examples/README.md'); copy('examples/templates.json');
  for (const template of Object.keys(JSON.parse(readFileSync(join(root, 'examples/templates.json'))))) copyTemplate(template, join(bundle, 'examples', template));
  for (const name of ['sdk', 'core', 'passkey', 'base']) copy(`packages/${name}/README.md`);
  mkdirSync(join(bundle, 'scripts')); copy('scripts/create-example.mjs'); copy('scripts/check-testnet-chain.mjs');
  mkdirSync(join(bundle, 'artifacts'));
  for (const pkg of packages.values()) cpSync(pkg.file, join(bundle, 'artifacts', pkg.filename));
  writeFileSync(join(bundle, 'package.json'), JSON.stringify({ name: 'z-stack-preview', private: true, version,
    scripts: { 'create:example': 'node scripts/create-example.mjs' }, engines: { node: '>=22.18.0' } }, null, 2) + '\n');
  const manifest = { version, revision, dirty: changes.length > 0,
    packages: Object.fromEntries([...packages].map(([name, { filename, version, sha256 }]) => [name, { filename, version, sha256 }])) };
  writeFileSync(join(bundle, 'PREVIEW.json'), JSON.stringify(manifest, null, 2) + '\n');
  writeFileSync(join(bundle, 'README.md'), `# z-stack preview ${version}\n\nSource revision: ${revision}${changes.length ? ' (local uncommitted rehearsal)' : ''}.\n\nUse Node 22.18+; no Rust toolchain or npm scope is needed for these built archives.\n\n1. Unpack this bundle into an ordinary local directory.\n2. Run \`node scripts/create-example.mjs browser-wallet /path/to/my-wallet --install\`. Use \`next-wallet\` for Next.js.\n3. In the generated app, run \`npm run dev\`. Use disposable test funds only.\n4. Follow [the integration walkthrough](docs/GETTING-STARTED.md). Create, reload and sync are verified on public testnet. Use [the Valar faucet](https://faucet.testnet.valargroup.dev/) for test coins; daily limits apply. An external alpha.6 run reported a Valar-funded public receive; outgoing public spending remains unverified. [Funded testing](docs/GETTING-STARTED.md#funded-testing) describes the limitation and the separate source-checkout regtest path.\n\nThe offline API starts at [docs/api/index.html](docs/api/index.html). PREVIEW.json records archive hashes and the source revision. Notices are included in every package. Source-build instructions and patch links in the deeper docs refer to the repository checkout.\n\nAccount and backup services are optional and configured by your application.\n`);
  const files = [];
  function inventory(dir, relative = '') {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name), key = relative + entry.name;
      assert.ok(!entry.isSymbolicLink(), 'Preview must not contain symbolic links');
      if (entry.isDirectory()) inventory(path, key + '/');
      else files.push(`${createHash('sha256').update(readFileSync(path)).digest('hex')}  ${key}`);
    }
  }
  inventory(bundle); writeFileSync(join(bundle, 'SHA256SUMS'), files.sort().join('\n') + '\n');
  const result = spawnSync('tar', ['-czf', output, '-C', scratch, 'z-stack-preview'], { stdio: 'inherit' });
  assert.equal(result.status, 0, 'tar failed: install a tar command (included on current Windows/macOS/Linux)');
  const filename = output.split(/[\\/]/).at(-1);
  writeFileSync(output + '.sha256', `${createHash('sha256').update(readFileSync(output)).digest('hex')}  ${filename}\n`);
  console.log(`Preview bundle: ${output}`);
} finally { rmSync(scratch, { recursive: true, force: true }); }
