// Per-replica trust for the local recorded-content proxy. These keys never sign
// releases or leave the attempt runtime; the owner removes the returned tree.
import {spawn} from 'node:child_process';
import {chmod, mkdtemp, readFile, rm} from 'node:fs/promises';
import {isIP} from 'node:net';
import {join} from 'node:path';

function runOpenSsl(openssl, arguments_) {
  return new Promise((resolve, reject) => {
    let settled = false;
    let timedOut = false;
    const child = spawn(openssl, arguments_, {stdio: 'ignore', windowsHide: true});
    const finish = (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      error ? reject(error) : resolve();
    };
    const timeout = setTimeout(() => {
      timedOut = true;
      child.kill();
    }, 15_000);
    child.once('error', () => finish(new Error('OpenSSL certificate generation could not start')));
    child.once('close', (code) => {
      if (timedOut) finish(new Error('OpenSSL certificate generation timed out'));
      else if (code === 0) finish();
      else finish(new Error('OpenSSL certificate generation failed'));
    });
  });
}

function sanFor(host) {
  if (typeof host !== 'string' || !host || host.length > 253) throw new TypeError('Claude WebFetch proxy hostname is invalid');
  const bare = host.startsWith('[') && host.endsWith(']') ? host.slice(1, -1) : host;
  if (isIP(bare)) return `IP:${bare}`;
  if (!bare.split('.').every(label => /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/i.test(label)))
    throw new TypeError('Claude WebFetch proxy hostname is invalid');
  return `DNS:${bare.toLowerCase()}`;
}

export async function createWebFetchCertificate(runtime, plan, {openssl = 'openssl'} = {}) {
  if (typeof runtime !== 'string' || !runtime || !Array.isArray(plan) ||
      typeof openssl !== 'string' || !openssl) throw new TypeError('Claude WebFetch certificate configuration is invalid');
  const hosts = new Set(['api.anthropic.com']);
  for (const item of plan) {
    let url;
    try { url = new URL(item.url); } catch { throw new Error('Claude WebFetch certificate plan has an invalid URL'); }
    if (!['http:', 'https:'].includes(url.protocol)) throw new Error('Claude WebFetch certificate plan has an unsupported URL');
    hosts.add(url.hostname);
  }
  const names = [...new Set([...hosts].map(sanFor))];
  if (names.length > 128) throw new Error('Claude WebFetch certificate has too many recorded hosts');
  const directory = await mkdtemp(join(runtime, 'webfetch-tls-'));
  const keyPath = join(directory, 'key.pem'), certPath = join(directory, 'cert.pem');
  try {
    // OpenSSL's entropy progress can exceed a small bounded pipe while adding
    // no actionable diagnostic. Run it without a shell or captured output.
    await runOpenSsl(openssl, ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', keyPath,
      '-out', certPath, '-days', '1', '-subj', '/CN=ASB local replay',
      '-addext', `subjectAltName=${names.join(',')}`]);
    await chmod(keyPath, 0o600);
    await chmod(certPath, 0o644);
    // The Claude guest can traverse and trust the certificate but cannot read
    // the service-owned key when running under its separate execution identity.
    await chmod(directory, 0o755);
    const [key, cert] = await Promise.all([readFile(keyPath), readFile(certPath)]);
    return {directory, keyPath, certPath, key, cert};
  } catch (error) {
    await rm(directory, {recursive: true, force: true}).catch(() => {});
    throw new Error('Claude WebFetch could not create a local replay certificate; install OpenSSL and retry', {cause: error});
  }
}
