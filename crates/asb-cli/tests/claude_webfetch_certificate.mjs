import assert from 'node:assert/strict';
import {mkdtemp, readFile, readdir, rm, stat} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {X509Certificate} from 'node:crypto';
import {createWebFetchCertificate} from '../assets/scripts/claude-webfetch-certificate.mjs';

const root = await mkdtemp(join(tmpdir(), 'asb-fetch-certificate-'));
try {
  const plan = [{url: 'https://example.test/path'}, {url: 'https://127.0.0.1/fixture'}, {url: 'https://example.test/other'}];
  const instances = await Promise.all(Array.from({length: 4}, () => createWebFetchCertificate(root, plan)));
  assert.equal(new Set(instances.map(item => item.directory)).size, 4);
  assert.equal(new Set(instances.map(item => item.cert.toString('hex'))).size, 4);
  for (const item of instances) {
    const certificate = new X509Certificate(item.cert);
    assert.ok(certificate.checkHost('example.test'));
    assert.ok(certificate.checkHost('api.anthropic.com'));
    assert.ok(certificate.checkIP('127.0.0.1'));
    assert.equal(certificate.checkHost('not-recorded.test'), undefined);
    assert.equal((await stat(item.keyPath)).mode & 0o777, 0o600);
    assert.equal((await stat(item.certPath)).mode & 0o777, 0o644);
    assert.deepEqual(await readFile(item.certPath), item.cert);
  }
  const before = new Set(await readdir(root));
  await assert.rejects(createWebFetchCertificate(root, plan, {openssl: join(root, 'missing-openssl')}), /install OpenSSL/);
  assert.deepEqual(new Set(await readdir(root)), before, 'failed generation must remove its temporary key directory');
  await assert.rejects(createWebFetchCertificate(root, [{url: 'file:///fixture'}]), /unsupported/);
  console.log('Claude WebFetch per-replica temporary certificates passed');
} finally { await rm(root, {recursive: true, force: true}); }
