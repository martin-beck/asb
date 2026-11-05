// Per-replica, offline replay of Claude WebFetch transport. TLS key material
// and validated cassette-derived plan are supplied by the owning adapter.
import http from 'node:http';
import tls from 'node:tls';

export function createWebFetchProxy({plan, key, cert}) {
  if (!Array.isArray(plan) || !key || !cert) throw new TypeError('Claude WebFetch proxy configuration is invalid');
  const queue = new Map(), ordinals = new Set();
  for (const item of plan) {
    let url;
    try { url = new URL(item.url); } catch { throw new Error('Claude WebFetch proxy URL is invalid'); }
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.hash ||
        !Number.isSafeInteger(item.ordinal) || item.ordinal < 0 ||
        !((item.status === 200 && typeof item.page === 'string') ||
          ([301, 302, 303, 307, 308].includes(item.status) && item.redirect === true)))
      throw new Error('Claude WebFetch proxy plan is invalid');
    if (ordinals.has(item.ordinal)) throw new Error('Claude WebFetch proxy has duplicate ordinals');
    ordinals.add(item.ordinal);
    const pending = queue.get(url.href) ?? [];
    // Without a per-call transport token, different responses for the same URL
    // cannot be assigned safely to concurrent Claude calls.
    if (pending.some(previous => previous.status !== item.status || previous.page !== item.page))
      throw new Error('Claude WebFetch proxy has ambiguous repeated URLs');
    pending.push(item); queue.set(url.href, pending);
  }
  const tlsContext = tls.createSecureContext({key, cert});
  const sockets = new Set();
  let failure, listening = false, closed = false;
  const fail = message => { failure ??= new Error(message); };
  function respond(req, res, authority) {
    let target;
    try { target = new URL(authority ? `https://${authority}${req.url}` : req.url); }
    catch { fail('Claude WebFetch proxy request is malformed'); res.writeHead(400, {'Connection': 'close'}).end(); return; }
    const pending = queue.get(target.href);
    if (!queue.has(target.href) && authority && target.hostname === 'api.anthropic.com' &&
        ['/api/web/domain_info', '/mcp-registry/v0/servers', '/api/event_logging/v2/batch'].includes(target.pathname)) {
      // Claude performs a domain-info request before fetch and may also issue
      // optional registry/diagnostic calls. None needs a live connection.
      const body = target.pathname === '/api/web/domain_info' ? '{"can_fetch":true}' : '{}';
      res.writeHead(200, {'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(body), 'Connection': 'close'}).end(body);
      return;
    }
    if (req.method !== 'GET' || !pending?.length) {
      fail('Claude WebFetch proxy request has no recorded response');
      res.writeHead(502, {'Connection': 'close'}).end(); return;
    }
    const item = pending.shift();
    if (item.redirect) {
      const redirect = target.hostname === 'asb-replay.invalid' ? 'https://asb-redirect.invalid/' : 'https://asb-replay.invalid/';
      res.writeHead(item.status, {'Location': redirect, 'Content-Length': '0', 'Connection': 'close'}).end();
      return;
    }
    const body = Buffer.from(item.page, 'utf8');
    res.writeHead(200, {'Content-Type': 'text/plain; charset=utf-8', 'Content-Length': body.length, 'Connection': 'close'}).end(body);
  }
  const inner = http.createServer();
  inner.on('request', (req, res) => respond(req, res, req.socket.asbAuthority));
  inner.on('clientError', (_error, socket) => { fail('Claude WebFetch TLS request is malformed'); socket.destroy(); });
  const proxy = http.createServer((req, res) => respond(req, res));
  proxy.on('connection', socket => { sockets.add(socket); socket.once('close', () => sockets.delete(socket)); });
  proxy.on('clientError', (_error, socket) => { fail('Claude WebFetch proxy request is malformed'); socket.destroy(); });
  proxy.on('connect', (req, socket, head) => {
    let target;
    try { target = new URL(`https://${req.url}`); } catch { fail('Claude WebFetch tunnel target is malformed'); socket.destroy(); return; }
    const permitted = target.hostname === 'api.anthropic.com' || [...queue.keys()].some(url => new URL(url).host === target.host);
    if (!permitted || target.pathname !== '/' || target.search || target.hash || target.username || target.password) {
      fail('Claude WebFetch tunnel has no recorded target'); socket.end('HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n'); return;
    }
    socket.write('HTTP/1.1 200 Connection Established\r\n\r\n');
    if (head.length) socket.unshift(head);
    const secure = new tls.TLSSocket(socket, {isServer: true, secureContext: tlsContext});
    secure.asbAuthority = target.host;
    secure.on('error', () => { fail('Claude WebFetch proxy TLS failed'); secure.destroy(); });
    inner.emit('connection', secure);
  });
  async function close() {
    closed = true;
    for (const socket of sockets) socket.destroy();
    if (listening) await new Promise((resolve, reject) => proxy.close(error => error ? reject(error) : resolve()));
  }
  return {
    async listen() {
      if (closed || listening) throw new Error('Claude WebFetch proxy is unavailable');
      await new Promise((resolve, reject) => { proxy.once('error', reject); proxy.listen(0, '127.0.0.1', resolve); });
      listening = true;
      return `http://127.0.0.1:${proxy.address().port}`;
    },
    async finish() {
      const incomplete = [...queue.values()].some(items => items.length);
      await close();
      if (failure) throw failure;
      if (incomplete) throw new Error('Claude WebFetch proxy has unconsumed recorded responses');
    },
    async abort() { if (!closed) await close(); },
  };
}
