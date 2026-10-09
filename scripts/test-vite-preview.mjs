import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {createRequire} from 'node:module';
import {createServer} from 'node:net';
import {dirname, join} from 'node:path';
import {readdirSync} from 'node:fs';

export async function verifyVitePreview(app) {
  const reservation = createServer();
  await new Promise(resolve => reservation.listen(0, '127.0.0.1', resolve));
  const port = reservation.address().port;
  await new Promise(resolve => reservation.close(resolve));
  const require = createRequire(join(app, 'package.json'));
  const bin = join(dirname(require.resolve('vite/package.json')), 'bin/vite.js');
  const child = spawn(process.execPath, [bin, 'preview', '--host', '127.0.0.1', '--port', String(port), '--strictPort'],
    {cwd: app, stdio: 'ignore'});
  const origin = `http://127.0.0.1:${port}`;
  const get = path => fetch(origin + path, {signal: AbortSignal.timeout(5000)});
  try {
    let html;
    for (let attempt = 0; attempt < 100; attempt++) {
      assert.equal(child.exitCode, null, 'Vite preview exited before readiness');
      try { html = await get('/'); if (html.ok) break; } catch {}
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    assert.ok(html?.ok, 'Vite preview did not start');
    assert.equal(html.headers.get('cross-origin-opener-policy'), 'same-origin');
    assert.equal(html.headers.get('cross-origin-embedder-policy'), 'credentialless');
    assert.doesNotMatch(html.headers.get('cache-control') ?? '', /immutable/);
    assert.ok(!html.headers.get('cache-control') || /(?:no-cache|no-store|max-age=0)(?:[,\s]|$)/.test(html.headers.get('cache-control')),
      'HTML must remain uncached or require revalidation');
    const files = readdirSync(join(app, 'dist/assets'));
    for (const extension of ['wasm', 'js', 'css']) {
      const asset = files.find(file => file.endsWith('.' + extension));
      assert.ok(asset, `Missing ${extension} build asset`);
      const response = await get('/assets/' + asset);
      assert.equal(response.status, 200);
      assert.equal(response.headers.get('cache-control'), 'public, max-age=31536000, immutable');
      if (extension === 'wasm') assert.match(response.headers.get('content-type'), /application\/wasm/);
      await response.arrayBuffer();
      const etag = response.headers.get('etag');
      if (etag) {
        const cached = await fetch(origin + '/assets/' + asset,
          {headers:{'If-None-Match':etag}, signal:AbortSignal.timeout(5000)});
        assert.equal(cached.status, 304);
        assert.match(cached.headers.get('cache-control'), /immutable/);
      }
    }
    const missing = await get('/assets/missing-AbCd1234.js');
    assert.doesNotMatch(missing.headers.get('cache-control') ?? '', /immutable/,
      'Missing assets or their HTML fallback must not be cached as immutable');
    console.log('Vite production preview: isolation, immutable hashed assets, fresh HTML and missing paths verified');
  } finally {
    if (child.exitCode === null) await new Promise(resolve => {
      child.once('exit', resolve); child.kill('SIGTERM');
      const timer = setTimeout(() => child.kill('SIGKILL'), 5000); timer.unref();
      child.once('exit', () => clearTimeout(timer));
    });
  }
}
