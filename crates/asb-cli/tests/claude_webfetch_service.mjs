import assert from 'node:assert/strict';
import {mkdtemp, readFile, readdir, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import http from 'node:http';
import tls from 'node:tls';
import {startWebFetchReplayService} from '../assets/scripts/claude-webfetch-service.mjs';

const root = await mkdtemp(join(tmpdir(), 'asb-fetch-service-'));
async function get(service, path) {
  const proxy = new URL(service.address), ca = await readFile(service.certPath);
  return new Promise((resolve, reject) => {
    const req = http.request({host: proxy.hostname, port: proxy.port, method: 'CONNECT', path: 'example.test:443'});
    req.on('connect', (response, socket) => {
      if (response.statusCode !== 200) { socket.destroy(); reject(new Error('tunnel failed')); return; }
      const secure = tls.connect({socket, servername: 'example.test', ca}, () => secure.write(`GET ${path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n`));
      let text = ''; secure.setEncoding('utf8'); secure.on('data', data => text += data);
      secure.on('end', () => resolve(text)); secure.on('error', reject);
    });
    req.on('error', reject); req.end();
  });
}
try {
  const plan = [{ordinal: 0, url: 'https://example.test/page', status: 200, page: 'fixture'}];
  const services = await Promise.all(Array.from({length: 4}, () => startWebFetchReplayService({runtime: root, plan})));
  assert.equal(new Set(services.map(service => service.address)).size, 4);
  await Promise.all(services.map(async service => {
    assert.match(await get(service, '/page'), /fixture/);
    await service.finish();
    await assert.rejects(readFile(service.certPath), {code: 'ENOENT'});
    await assert.rejects(service.finish(), /already closed/);
  }));
  assert.deepEqual(await readdir(root), []);
  const incomplete = await startWebFetchReplayService({runtime: root, plan});
  await assert.rejects(incomplete.finish(), /unconsumed/);
  await assert.rejects(readFile(incomplete.certPath), {code: 'ENOENT'});
  await assert.rejects(startWebFetchReplayService({runtime: root, plan: [{url: 'https://example.test/', ordinal: 0, status: 500}]}), /plan is invalid/);
  assert.deepEqual(await readdir(root), []);
  const cancelled = await startWebFetchReplayService({runtime: root, plan});
  await cancelled.abort();
  assert.deepEqual(await readdir(root), []);
  console.log('Claude WebFetch service: per-replica lifetime and cleanup passed');
} finally { await rm(root, {recursive: true, force: true}); }
