import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {createRequire} from 'node:module';
import {createServer} from 'node:net';
import {dirname, join} from 'node:path';
import {readdirSync, readFileSync, statSync} from 'node:fs';
import {get as httpGet} from 'node:http';
import {gunzipSync} from 'node:zlib';

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
    for (const [extension, asset] of [...files.filter(file => file.endsWith('.wasm')).map(file => ['wasm',file]),
      ...['js','css'].map(extension => [extension,files.find(file => file.endsWith('.'+extension) && (extension !== 'js' || statSync(join(app,'dist/assets',file)).size > 1024))])]) {
      assert.ok(asset, `Missing ${extension} build asset`);
      const response = await get('/assets/' + asset);
      assert.equal(response.status, 200);
      assert.equal(response.headers.get('cache-control'), 'public, max-age=31536000, immutable');
      if (extension === 'wasm') assert.match(response.headers.get('content-type'), /application\/wasm/);
      await response.arrayBuffer();
      const head = await fetch(origin + '/assets/' + asset, {method:'HEAD', signal:AbortSignal.timeout(5000)});
      assert.equal(head.status, 200);
      assert.equal(head.headers.get('cache-control'), 'public, max-age=31536000, immutable');
      if (extension === 'wasm' || extension === 'js') {
        const raw = headers => new Promise((resolve, reject) => {
          const request = httpGet(origin + '/assets/' + asset, {headers}, response => {
            const chunks = [];
            response.on('data', chunk => chunks.push(chunk));
            response.on('end', () => resolve({headers:response.headers, bytes:Buffer.concat(chunks)}));
            response.on('error', reject);
          });
          request.setTimeout(5000, () => request.destroy(new Error('preview request timed out')));
          request.on('error', reject);
        });
        const original = readFileSync(join(app, 'dist/assets', asset));
        const compressed = await raw({'Accept-Encoding':'gzip'});
        assert.equal(compressed.headers['content-encoding'], 'gzip');
        assert.match(compressed.headers.vary, /Accept-Encoding/i);
        assert.deepEqual(gunzipSync(compressed.bytes), original, `compressed ${extension} changed original bytes`);
        assert.ok(compressed.bytes.length < original.length / (extension === 'wasm' ? 2 : 1), `${extension} was not compressed on the wire`);
        for (const encoding of extension === 'wasm' ? ['identity', 'gzip;q=0, identity'] : []) {
          const plain = await raw({'Accept-Encoding':encoding});
          assert.equal(plain.headers['content-encoding'], undefined);
          assert.deepEqual(plain.bytes, original);
        }
      }
      const etag = response.headers.get('etag');
      if (etag) {
        const cached = await fetch(origin + '/assets/' + asset,
          {headers:{'If-None-Match':etag}, signal:AbortSignal.timeout(5000)});
        assert.equal(cached.status, 304);
        assert.match(cached.headers.get('cache-control'), /immutable/);
      }
    }
    const missing = await get('/assets/missing-AbCd1234.js');
    assert.equal(missing.status, 404);
    for (const path of ['/missing.wasm', '/assets/missing-AbCd1234.wasm', '/assets/missing-worker-AbCd1234.js']) {
      for (const method of ['GET', 'HEAD']) {
        const absent = await fetch(origin + path, {method, signal:AbortSignal.timeout(5000)});
        assert.equal(absent.status, 404, `Missing ${path} must not return HTML`);
        assert.doesNotMatch(absent.headers.get('content-type') ?? '', /text\/html/);
        assert.equal(absent.headers.get('cache-control'), 'no-store');
      }
    }
    assert.doesNotMatch(missing.headers.get('cache-control') ?? '', /immutable/,
      'Missing assets or their HTML fallback must not be cached as immutable');
    const fallbackEtag = missing.headers.get('etag');
    if (fallbackEtag) {
      const conditionalMissing = await fetch(origin + '/assets/missing-AbCd1234.js',
        {headers:{'If-None-Match':fallbackEtag}, signal:AbortSignal.timeout(5000)});
      assert.doesNotMatch(conditionalMissing.headers.get('cache-control') ?? '', /immutable/,
        'A conditional HTML fallback must not become immutable');
    }
    console.log('Vite production preview: isolation, WASM/JS gzip byte integrity, WASM identity negotiation, GET/HEAD/304 cache, fresh HTML and missing paths verified');
  } finally {
    if (child.exitCode === null) await new Promise(resolve => {
      child.once('exit', resolve); child.kill('SIGTERM');
      const timer = setTimeout(() => child.kill('SIGKILL'), 5000); timer.unref();
      child.once('exit', () => clearTimeout(timer));
    });
  }
}
