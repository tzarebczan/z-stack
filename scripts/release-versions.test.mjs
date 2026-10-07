import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { assertReleaseVersions } from './release-versions.mjs';

test('reject mismatched archive/bundle/runtime labels before release', () => {
  const root = mkdtempSync(join(tmpdir(), 'z-stack-versions-'));
  const version = '0.1.0-alpha.1';
  const write = (file, content) => {
    mkdirSync(dirname(join(root, file)), { recursive: true });
    writeFileSync(join(root, file), content);
  };
  const manifests = ['package.json', ...['sdk', 'core', 'passkey', 'base'].map(name => `packages/${name}/package.json`)];
  const cargo = `[workspace.package]\n# version = "wrong-comment"\nversion = "${version}" # release\n\n[workspace.dependencies]\nfoo = "9.9.9"\n`;
  const runtime = `export const SDK_VERSION = "${version}";\n`;
  try {
    for (const file of manifests) write(file, JSON.stringify({ version }));
    write('Cargo.toml', cargo);
    write('packages/sdk/src/runtime.ts', runtime);
    assert.equal(assertReleaseVersions(root), version);
    for (const file of manifests) {
      write(file, JSON.stringify({ version: '0.2.0-alpha.1' }));
      assert.throws(() => assertReleaseVersions(root), /version must match|must match SDK|SDK_VERSION/);
      write(file, JSON.stringify({ version }));
    }
    write('Cargo.toml', cargo.replace(`version = "${version}"`, 'version = "0.2.0-alpha.1"'));
    assert.throws(() => assertReleaseVersions(root), /Cargo workspace/);
    write('Cargo.toml', cargo);
    write('packages/sdk/src/runtime.ts', runtime.replace(version, '0.2.0-alpha.1'));
    assert.throws(() => assertReleaseVersions(root), /SDK_VERSION/);
    write('packages/sdk/src/runtime.ts', runtime);
    assert.equal(assertReleaseVersions(root), version);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
