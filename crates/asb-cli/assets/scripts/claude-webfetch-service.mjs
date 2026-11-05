// Adapter-owned lifetime for one replica's private recorded-content proxy.
import {rm} from 'node:fs/promises';
import {createWebFetchCertificate} from './claude-webfetch-certificate.mjs';
import {createWebFetchProxy} from './claude-webfetch-proxy.mjs';

export async function startWebFetchReplayService({runtime, plan, openssl}) {
  const identity = await createWebFetchCertificate(runtime, plan, {openssl: openssl ?? 'openssl'});
  let proxy;
  try {
    proxy = createWebFetchProxy({plan, key: identity.key, cert: identity.cert});
    const address = await proxy.listen();
    let closed = false;
    async function close(settle) {
      if (closed) throw new Error('Claude WebFetch replay service is already closed');
      closed = true;
      try {
        if (settle) await proxy.finish();
        else await proxy.abort();
      } finally {
        await rm(identity.directory, {recursive: true, force: true});
      }
    }
    return {address, certPath: identity.certPath, finish: () => close(true), abort: () => close(false)};
  } catch (error) {
    if (proxy) await proxy.abort().catch(() => {});
    await rm(identity.directory, {recursive: true, force: true}).catch(() => {});
    throw error;
  }
}
