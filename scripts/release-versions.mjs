// Release archives, their bundle label and runtime must describe the same version.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

export function assertReleaseVersions(root) {
  const manifest = path => JSON.parse(readFileSync(join(root, path), 'utf8'));
  const version = manifest('packages/sdk/package.json').version;
  assert.equal(typeof version, 'string', 'SDK version must be declared');
  for (const file of ['package.json', ...['core', 'passkey', 'base'].map(name => `packages/${name}/package.json`)]) {
    assert.equal(manifest(file).version, version, `${file}: release version must match SDK ${version}`);
  }
  const cargo = readFileSync(join(root, 'Cargo.toml'), 'utf8');
  const workspace = cargo.match(/^\[workspace\.package\]\s*\n([\s\S]*?)(?=^\[|$(?![\s\S]))/m)?.[1];
  const cargoVersion = workspace?.match(/^version\s*=\s*"([^"]+)"\s*(?:#.*)?$/m)?.[1];
  assert.equal(cargoVersion, version, 'Cargo workspace release version must match SDK');
  const runtime = readFileSync(join(root, 'packages/sdk/src/runtime.ts'), 'utf8');
  const runtimeVersion = runtime.match(/^export const SDK_VERSION = "([^"]+)";/m)?.[1];
  assert.equal(runtimeVersion, version, 'SDK_VERSION must match package version');
  return version;
}
