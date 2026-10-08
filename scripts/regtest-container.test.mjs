import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, readFile, access, rm } from 'node:fs/promises';
import { once } from 'node:events';
import { execFile, spawn } from 'node:child_process';
import { promisify } from 'node:util';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const exec = promisify(execFile);
const modulePath = resolve('scripts/regtest-container.mjs');
const id = 'a'.repeat(64);
const linuxFixture = process.platform === 'linux' && process.arch === 'x64';
async function fixture(fn) {
  const dir = await mkdtemp(join(tmpdir(), 'z-stack-container-'));
  await writeFile(join(dir, 'docker'), `#!/usr/bin/env node
const fs = require('fs');
const args = process.argv.slice(2);
fs.appendFileSync(process.env.CALLS, JSON.stringify(args)+'\\n');
if(args[0]==='inspect'){
 if(process.env.FAIL){console.error(process.env.FAIL); process.exit(1);}
 console.log(JSON.stringify([{Id:'${id}',Config:{Image:'fixture:one',Labels:{'io.z-stack.regtest.directory':process.env.OWNER}},State:{Running:true}}]));
} else if(args[0]==='run') console.log('${id}');
`, { mode: 0o755 });
  const env = { ...process.env, PATH: `${dir}:${process.env.PATH}`, CALLS: join(dir, 'calls'), OWNER: dir };
  try { await fn(dir, env); } finally { await rm(dir, { recursive: true, force: true }); }
}
function call(dir, env, code) { return exec(process.execPath, ['--input-type=module', '-e', `import * as c from ${JSON.stringify(modulePath)}; const dir=${JSON.stringify(dir)}; ${code}`], { env }); }

test('container cleanup refuses a different fixture and daemon failures preserve ownership', { skip: !linuxFixture }, async () => {
 await fixture(async (dir, env) => {
  const file = join(dir, 'zaino.container');
  await writeFile(file, id);
  await assert.rejects(call(dir, {...env, OWNER:'/other/fixture'}, 'c.stopContainer(dir)'), /ownership does not match/);
  await assert.rejects(call(dir, {...env, FAIL:'Cannot connect to the Docker daemon'}, 'c.stopContainer(dir)'), /Cannot inspect/);
  assert.equal(await readFile(file,'utf8'), id);
  const calls = (await readFile(env.CALLS,'utf8')).trim().split('\n').map(JSON.parse);
  assert.ok(calls.every(args=>args[0]==='inspect'));
 });
});
test('missing containers clear only the stale marker and live image changes are refused', { skip: !linuxFixture }, async () => {
 await fixture(async (dir, env) => {
  const file=join(dir,'zaino.container'); await writeFile(file,id);
  await assert.rejects(call(dir,env,'c.containerRunning(dir,"fixture:two")'), /different Zaino image/);
  await call(dir,{...env,FAIL:`Error: No such object: ${id}`}, 'c.stopContainer(dir)');
  await assert.rejects(access(file), /ENOENT/);
 });
});
test('container startup mounts only config and its data with non-root restricted runtime', { skip: !linuxFixture || process.getuid?.() === 0 }, async () => {
 await fixture(async (dir, env) => {
  await call(dir,env,'c.startContainer(dir,"fixture:one")');
  assert.equal(await readFile(join(dir,'zaino.container'),'utf8'),id);
  const args=JSON.parse((await readFile(env.CALLS,'utf8')).trim());
  for(const arg of ['--network=host','--read-only','--cap-drop=ALL','--security-opt=no-new-privileges']) assert.ok(args.includes(arg));
  const mounts=args.filter((_,i)=>args[i-1]==='--mount');
  assert.deepEqual(mounts,[`type=bind,source=${dir}/zaino.toml,target=${dir}/zaino.toml,readonly`,`type=bind,source=${dir}/zaino,target=${dir}/zaino`]);
  assert.ok(args.includes(`${process.getuid()}:${process.getgid()}`));
 });
});

test('down stops the owned validator even when Docker is unavailable and retains its container marker', { skip: !linuxFixture }, async () => {
 await fixture(async (dir, env) => {
  const validator=spawn(process.execPath, ['-e','setInterval(()=>{},1000)'], {stdio:'ignore'});
  const exited=once(validator,'exit');
  try {
   await writeFile(join(dir,'zakura.pid'),String(validator.pid));
   await writeFile(join(dir,'zaino.container'),id);
   await assert.rejects(exec(process.execPath,['scripts/regtest-native.mjs','down'],{
    env:{...env,Z_STACK_REGTEST_DIR:dir,FAIL:'Cannot connect to the Docker daemon'},timeout:15000
   }),/Cannot inspect fixture container/);
   assert.equal((await exited)[1],'SIGTERM');
   await assert.rejects(access(join(dir,'zakura.pid')),/ENOENT/);
   assert.equal(await readFile(join(dir,'zaino.container'),'utf8'),id);
  } finally {validator.kill();await exited;}
 });
});
