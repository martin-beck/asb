import assert from 'node:assert/strict';
import {mkdtemp, readFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {spawnSync} from 'node:child_process';
import http from 'node:http';
import tls from 'node:tls';
import {createWebFetchProxy} from '../assets/scripts/claude-webfetch-proxy.mjs';

const root = await mkdtemp(join(tmpdir(), 'asb-webfetch-proxy-'));
try {
  const keyPath = join(root, 'key.pem'), certPath = join(root, 'cert.pem');
  const generated = spawnSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', keyPath, '-out', certPath, '-days', '1', '-subj', '/CN=example.test', '-addext', 'subjectAltName=DNS:example.test,DNS:api.anthropic.com'], {stdio: 'ignore'});
  assert.equal(generated.status, 0, 'openssl must generate the local test certificate');
  const key = await readFile(keyPath), cert = await readFile(certPath);
  async function get(proxyUrl, host, path) {
    const proxy = new URL(proxyUrl);
    return new Promise((resolve, reject) => {
      const request = http.request({host: proxy.hostname, port: proxy.port, method: 'CONNECT', path: `${host}:443`});
      request.on('connect', (response, socket, head) => {
        if (response.statusCode !== 200) { socket.destroy(); reject(new Error(`tunnel status ${response.statusCode}`)); return; }
        if (head.length) socket.unshift(head);
        const secure = tls.connect({socket, servername: host, ca: cert}, () => secure.write(`GET ${path} HTTP/1.1\r\nHost: ${host}\r\nConnection: close\r\n\r\n`));
        let data = ''; secure.setEncoding('utf8'); secure.on('data', chunk => data += chunk);
        secure.on('end', () => resolve(data)); secure.on('error', reject);
      });
      request.on('error', reject); request.end();
    });
  }
  async function replica() {
    const proxy = createWebFetchProxy({key, cert, plan: [
      {ordinal: 1, url: 'https://example.test/page?q=1', status: 200, page: '<html>fixture café</html>'},
      {ordinal: 2, url: 'https://example.test/redirect', status: 302, redirect: true},
    ]});
    try {
      const url = await proxy.listen();
      const check = await get(url, 'api.anthropic.com', '/api/web/domain_info?domain=example.test');
      assert.match(check, /"can_fetch":true/);
      const page = await get(url, 'example.test', '/page?q=1');
      assert.match(page, /HTTP\/1\.1 200/); assert.match(page, /fixture café/);
      const redirect = await get(url, 'example.test', '/redirect');
      assert.match(redirect, /HTTP\/1\.1 302/); assert.match(redirect, /location: https:\/\/asb-replay\.invalid\//i);
      await proxy.finish();
    } finally { await proxy.abort(); }
  }
  await Promise.all([replica(), replica(), replica(), replica()]);
  assert.throws(() => createWebFetchProxy({key, cert, plan: [
    {ordinal: 0, url: 'https://example.test/same', status: 200, page: 'a'},
    {ordinal: 1, url: 'https://example.test/same', status: 200, page: 'b'},
  ]}), /ambiguous/);
  assert.throws(() => createWebFetchProxy({key, cert, plan: [
    {ordinal: 0, url: 'https://example.test/one', status: 200, page: 'a'},
    {ordinal: 0, url: 'https://example.test/two', status: 200, page: 'b'},
  ]}), /duplicate ordinals/);
  const apiFetch = createWebFetchProxy({key, cert, plan: [{ordinal: 0, url: 'https://api.anthropic.com/recorded', status: 200, page: 'recorded api content'}]});
  const apiUrl = await apiFetch.listen();
  assert.match(await get(apiUrl, 'api.anthropic.com', '/recorded'), /recorded api content/);
  await apiFetch.finish();
  const unknownApi = createWebFetchProxy({key, cert, plan: []});
  const unknownApiUrl = await unknownApi.listen();
  const unknownApiResponse = await get(unknownApiUrl, 'api.anthropic.com', '/unrecorded');
  assert.match(unknownApiResponse, /HTTP\/1\.1 502/);
  await assert.rejects(unknownApi.finish(), /no recorded response/);
  const plain = createWebFetchProxy({key, cert, plan: [{ordinal: 0, url: 'http://example.test/plain', status: 200, page: 'plain fixture'}]});
  const plainUrl = new URL(await plain.listen());
  const plainBody = await new Promise((resolve, reject) => {
    http.get({host: plainUrl.hostname, port: plainUrl.port, path: 'http://example.test/plain'}, response => {
      let text = ''; response.setEncoding('utf8'); response.on('data', chunk => text += chunk);
      response.on('end', () => resolve(text));
    }).on('error', reject);
  });
  assert.equal(plainBody, 'plain fixture');
  await plain.finish();
  const unknown = createWebFetchProxy({key, cert, plan: []});
  const unknownUrl = await unknown.listen();
  await assert.rejects(get(unknownUrl, 'example.test', '/not-recorded'), /tunnel status 502/);
  await assert.rejects(unknown.finish(), /no recorded target/);
  const incomplete = createWebFetchProxy({key, cert, plan: [{ordinal: 0, url: 'https://example.test/not-read', status: 200, page: 'x'}]});
  await incomplete.listen();
  await assert.rejects(incomplete.finish(), /unconsumed/);
  console.log('Claude WebFetch offline TLS proxy: four replicas, redirect and failure checks passed');
} finally { await rm(root, {recursive: true, force: true}); }
