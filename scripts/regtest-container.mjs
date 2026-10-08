/** Docker-backed Zaino for the native Linux loopback regtest fixture. */
import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

function docker(args) {
  const result = spawnSync('docker', args, { encoding: 'utf8' });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`docker ${args[0]} failed: ${result.stderr.trim()}`);
  return result.stdout.trim();
}

function ownedContainer(dir) {
  const file = join(dir, 'zaino.container');
  if (!existsSync(file)) return undefined;
  const id = readFileSync(file, 'utf8').trim();
  if (!/^[a-f0-9]{64}$/.test(id)) throw new Error('Invalid fixture container ID');
  const result = spawnSync('docker', ['inspect', id], { encoding: 'utf8' });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    // Only a genuinely missing container is stale; daemon/auth failures are not.
    if (/No such (object|container)/i.test(result.stderr)) { rmSync(file); return undefined; }
    throw new Error(`Cannot inspect fixture container: ${result.stderr.trim()}`);
  }
  const container = JSON.parse(result.stdout)[0];
  if (container.Config.Labels?.['io.z-stack.regtest.directory'] !== dir)
    throw new Error('Container ownership does not match this fixture');
  return container;
}

export function containerRunning(dir, image) {
  const container = ownedContainer(dir);
  if (container?.State.Running && container.Config.Image !== image)
    throw new Error('A different Zaino image is running; run down before changing it');
  return Boolean(container?.State.Running);
}

export function stopContainer(dir) {
  const container = ownedContainer(dir);
  if (!container) return false;
  if (container.State.Running) docker(['stop', '--time=20', container.Id]);
  docker(['rm', container.Id]);
  rmSync(join(dir, 'zaino.container'));
  return true;
}

export function startContainer(dir, image) {
  if (process.platform !== 'linux' || process.arch !== 'x64')
    throw new Error('The NU7 container fixture currently supports Linux x64 only');
  if (!process.getuid?.()) throw new Error('Run the local fixture as a non-root user');
  if (/[,"\r\n]/.test(dir)) throw new Error('Fixture directory cannot contain commas, quotes or newlines');
  if (!/^[a-zA-Z0-9][a-zA-Z0-9._/:@-]+$/.test(image)) throw new Error('Invalid Zaino image reference');
  if (containerRunning(dir, image)) return;
  stopContainer(dir);
  const id = docker(['run', '--detach', '--network=host', '--read-only',
    '--cap-drop=ALL', '--security-opt=no-new-privileges', '--pids-limit=256',
    '--user', `${process.getuid()}:${process.getgid()}`, '--env', 'HOME=/tmp',
    '--tmpfs', '/tmp:rw,noexec,nosuid,size=64m',
    '--label', `io.z-stack.regtest.directory=${dir}`,
    '--mount', `type=bind,source=${join(dir, 'zaino.toml')},target=${join(dir, 'zaino.toml')},readonly`,
    '--mount', `type=bind,source=${join(dir, 'zaino')},target=${join(dir, 'zaino')}`,
    image, 'start', '--config', join(dir, 'zaino.toml')]);
  if (!/^[a-f0-9]{64}$/.test(id)) throw new Error('Docker returned an invalid container ID');
  writeFileSync(join(dir, 'zaino.container'), id, { mode: 0o600 });
}
