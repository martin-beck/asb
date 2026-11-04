// A replay agent starts only after its sibling service has published the local
// proxy endpoint. Keep the user's existing extra CA certificates in the bundle.
import {spawn} from 'node:child_process';
import {open, readFile} from 'node:fs/promises';
import {join} from 'node:path';

export async function launchClaude({program, args, environment = process.env, stdio = 'inherit'}) {
  const state = environment.ASB_CLAUDE_NATIVE_DIRECTORY;
  const readyPath = environment.ASB_CLAUDE_WEB_READY;
  if (typeof program !== 'string' || !program || !Array.isArray(args) || !state || !readyPath)
    throw new Error('Claude replay launch configuration is incomplete');
  const ready = JSON.parse(await readFile(readyPath, 'utf8'));
  let address;
  try { address = new URL(ready.address); } catch { throw new Error('Claude replay proxy endpoint is invalid'); }
  if (address.protocol !== 'http:' || address.hostname !== '127.0.0.1' || !address.port ||
      address.pathname !== '/' || address.search || address.hash || typeof ready.certPath !== 'string' || !ready.certPath)
    throw new Error('Claude replay proxy endpoint is invalid');
  const cert = await readFile(ready.certPath);
  const prior = environment.NODE_EXTRA_CA_CERTS ? await readFile(environment.NODE_EXTRA_CA_CERTS) : Buffer.alloc(0);
  const bundle = join(state, '..', 'webfetch-trust.pem');
  const file = await open(bundle, 'wx', 0o600);
  try { await file.writeFile(Buffer.concat([prior, Buffer.from('\n'), cert])); } finally { await file.close(); }
  const noProxy = ['127.0.0.1', 'localhost', '::1'];
  if (environment.ANTHROPIC_BASE_URL) {
    try { noProxy.push(new URL(environment.ANTHROPIC_BASE_URL).hostname); }
    catch { throw new Error('Claude replay model endpoint is invalid'); }
  }
  const env = {...environment, HTTPS_PROXY: ready.address, HTTP_PROXY: ready.address,
    https_proxy: ready.address, http_proxy: ready.address, NO_PROXY: noProxy.join(','), no_proxy: noProxy.join(','),
    NODE_EXTRA_CA_CERTS: bundle, ASB_CLAUDE_NATIVE_WEB_PROXY: ready.address};
  const child = spawn(program, args, {env, stdio});
  const forward = signal => { if (!child.killed) child.kill(signal); };
  const onTerm = () => forward('SIGTERM'), onInt = () => forward('SIGINT');
  process.on('SIGTERM', onTerm); process.on('SIGINT', onInt);
  try {
    return await new Promise((resolve, reject) => {
      child.once('error', () => reject(new Error('Claude replay client could not start')));
      child.once('exit', (code, signal) => resolve({code, signal}));
    });
  } finally { process.off('SIGTERM', onTerm); process.off('SIGINT', onInt); }
}

export async function main(args = process.argv.slice(1)) {
  const [marker, program, ...rest] = args;
  if (marker !== '--asb-claude-executable') throw new Error('Claude replay client identity is missing');
  const result = await launchClaude({program, args: rest});
  if (result.signal) {
    const numbers = {SIGHUP: 1, SIGINT: 2, SIGTERM: 15};
    process.exitCode = 128 + (numbers[result.signal] ?? 1);
  } else process.exitCode = result.code ?? 1;
}
