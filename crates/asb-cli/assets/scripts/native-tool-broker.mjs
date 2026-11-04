// Adapter-owned IPC coordinator prototype. The owner controls socket lifetime,
// persistence and snapshot application; clients only exchange tool events.
import { createServer, connect } from 'node:net';
import { RecordingSession, ReplaySession, normalizeRequest } from './native-tool-session.mjs';

const MAX_MESSAGE = 16 * 1024 * 1024;
function frame(socket) {
  return new Promise((resolve, reject) => {
    let bytes = 0, data = '';
    socket.setEncoding('utf8');
    socket.on('data', chunk => {
      bytes += Buffer.byteLength(chunk);
      if (bytes > MAX_MESSAGE) { reject(new Error('Native tool broker message exceeds limit')); socket.destroy(); return; }
      data += chunk;
      const end = data.indexOf('\n');
      if (end >= 0) {
        try { resolve(JSON.parse(data.slice(0, end))); } catch { reject(new Error('Native tool broker message is malformed')); }
      }
    });
    socket.once('error', reject);
    socket.once('end', () => reject(new Error('Native tool broker connection ended early')));
  });
}
function reply(socket, value) { socket.end(`${JSON.stringify(value)}\n`); }

export class NativeToolBroker {
  constructor(mode, events = [], applyEffects) {
    if (!['record', 'replay'].includes(mode)) throw new Error('Native tool broker mode is invalid');
    this.mode = mode;
    this.session = mode === 'record' ? new RecordingSession() : new ReplaySession(events, applyEffects);
    this.pending = new Map();
    this.connections = new Set();
    this.sealing = false;
    this.nextToken = 0;
    this.server = createServer(socket => {
      this.connections.add(socket);
      socket.once('close', () => this.connections.delete(socket));
      this.handle(socket);
    });
  }
  async handle(socket) {
    try {
      const message = await frame(socket);
      if (this.sealing) throw new Error('Native tool broker is sealing its recording');
      if (this.mode === 'replay' && message.op === 'invoke') {
        try { reply(socket, { ok: true, value: await this.session.invoke(message.request) }); }
        catch (error) { reply(socket, { ok: false, name: error.name, message: error.message }); }
      } else if (this.mode === 'record' && message.op === 'start') {
        const request = normalizeRequest(message.request);
        const token = this.nextToken++;
        let resolve, reject;
        const pending = new Promise((yes, no) => { resolve = yes; reject = no; });
        const result = this.session.invoke(request, () => pending);
        // Keep the promise observed even if a client disconnects before finish.
        result.catch(() => {});
        this.pending.set(token, { resolve, reject, result });
        reply(socket, { ok: true, token });
      } else if (this.mode === 'record' && message.op === 'invalidate') {
        const item = this.pending.get(message.token);
        if (!item) throw new Error('Native tool broker invalidation has no active call');
        this.pending.delete(message.token);
        this.session.abort();
        item.reject(new Error('Native tool tool exchange could not be recorded'));
        try { await item.result; } catch { /* the recording is invalid */ }
        reply(socket, { ok: true });
      } else if (this.mode === 'record' && message.op === 'finish') {
        const item = this.pending.get(message.token);
        if (!item || !message.outcome || typeof message.outcome.ok !== 'boolean') throw new Error('Native tool broker finish has no active call');
        this.pending.delete(message.token);
        if (message.outcome.ok) item.resolve(message.outcome.value);
        else { const error = new Error(message.outcome.message); error.name = message.outcome.name; item.reject(error); }
        try { await item.result; } catch { /* recorded tool failure */ }
        reply(socket, { ok: true });
      } else throw new Error('Native tool broker operation is invalid');
    } catch (error) { if (!socket.destroyed) reply(socket, { ok: false, name: error.name, message: error.message }); }
  }
  listen(path) { return new Promise((resolve, reject) => { this.server.once('error', reject); this.server.listen(path, resolve); }); }
  async finish(encode) {
    if (this.sealing) throw new Error('Native tool broker is already sealing');
    this.sealing = true;
    let events, encoded;
    try {
      if (this.pending.size) throw new Error('Native tool broker has unfinished calls');
      if (encode) encoded = encode(this.session.preview());
      events = this.session.finish();
    } catch (error) {
      this.sealing = false;
      throw error;
    }
    await new Promise((resolve, reject) => this.server.close(error => error ? reject(error) : resolve()));
    return encode ? encoded : events;
  }
  async abort() {
    const error = new Error('Native tool broker was cancelled');
    this.session.abort(error);
    for (const item of this.pending.values()) item.reject(error);
    this.pending.clear();
    for (const socket of this.connections) socket.destroy();
    await new Promise(resolve => this.server.close(() => resolve()));
  }
}

export async function brokerRequest(path, message) {
  const socket = connect(path);
  const response = frame(socket);
  socket.once('connect', () => socket.write(`${JSON.stringify(message)}\n`));
  const result = await response;
  if (!result.ok) { const error = new Error(result.message); error.name = result.name || 'Error'; throw error; }
  return result;
}
