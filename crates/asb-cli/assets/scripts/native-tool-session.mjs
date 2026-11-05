// Native tool-specific native dispatch ordering. Storage and cross-process routing
// are supplied by the caller; this module never starts a tool during replay.
function canonical(value) {
  if (value === null || typeof value === 'boolean' || typeof value === 'string') return JSON.stringify(value);
  if (typeof value === 'number' && Number.isFinite(value)) return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (typeof value === 'object' && Object.getPrototypeOf(value) === Object.prototype) {
    return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`;
  }
  throw new TypeError('Native tool tool exchange is not representable as JSON');
}
function clone(value) { return JSON.parse(canonical(value)); }
export function normalizeRequest(value) { return clone(value); }
export function encodeThrown(value) {
  if (value === undefined) return { kind: 'undefined' };
  if (typeof value === 'bigint') return { kind: 'bigint', value: String(value) };
  return { kind: 'json', value: clone(value) };
}
export function decodeThrown(value) {
  if (value?.kind === 'undefined' && Object.keys(value).length === 1) return undefined;
  if (value?.kind === 'bigint' && Object.keys(value).length === 2 && typeof value.value === 'string' && /^-?(0|[1-9][0-9]*)$/.test(value.value)) return BigInt(value.value);
  if (value?.kind === 'json' && Object.keys(value).length === 2 && Object.hasOwn(value, 'value')) return clone(value.value);
  throw new TypeError('Native tool thrown value is invalid');
}

export class RecordingSession {
  constructor() { this.next = 0; this.completed = []; this.active = new Set(); this.closed = false; this.invalid = false; }
  async invoke(request, execute) {
    if (this.closed || typeof execute !== 'function') throw new Error('Native tool recording session is unavailable');
    const normalized = clone(request);
    const ordinal = this.next++;
    this.active.add(ordinal);
    let value;
    try {
      value = await execute();
    } catch (error) {
      try {
        const outcome = error instanceof Error ? { ok: false, name: error.name, message: error.message } : { ok: false, thrown: encodeThrown(error) };
        this.completed.push({ ordinal, request: normalized, outcome });
      } catch (encodingError) { this.invalid = true; throw encodingError; }
      throw error;
    } finally { this.active.delete(ordinal); }
    try {
      this.completed.push({ ordinal, request: normalized, outcome: { ok: true, value: clone(value) } });
    } catch (error) { this.invalid = true; throw error; }
    return value;
  }
  preview() {
    if (this.closed || this.invalid || this.active.size) throw new Error('Native tool recording has unfinished tool calls');
    return clone(this.completed);
  }
  finish() { const events = this.preview(); this.closed = true; return events; }
  abort() { this.invalid = true; this.closed = true; }
}

export class ReplaySession {
  constructor(events, applyEffects = async () => {}) {
    this.events = clone(events);
    if (!Array.isArray(this.events) || typeof applyEffects !== 'function') throw new TypeError('Native tool replay input is invalid');
    this.byDispatch = [...this.events].sort((a, b) => a.ordinal - b.ordinal);
    if (this.byDispatch.some((event, index) => event.ordinal !== index || !event.request || !event.outcome ||
        typeof event.outcome.ok !== 'boolean' ||
        (event.outcome.ok && !Object.hasOwn(event.outcome, 'value')) ||
        (!event.outcome.ok && !Object.hasOwn(event.outcome, 'thrown') && (typeof event.outcome.name !== 'string' || typeof event.outcome.message !== 'string')))) {
      throw new Error('Native tool replay events are incomplete');
    }
    for (const event of this.byDispatch) if (!event.outcome.ok && Object.hasOwn(event.outcome, 'thrown')) decodeThrown(event.outcome.thrown);
    this.applyEffects = applyEffects;
    this.nextDispatch = 0;
    this.nextCompletion = 0;
    this.pending = [];
    this.assigned = new Map();
    this.draining = false;
    this.closed = false;
  }
  // Reserve dispatch order now, but apply recorded effects only when the
  // adapter signals that its corresponding native completion hook has fired.
  reserve(request) {
    if (this.closed) throw new Error('Native tool replay session is closed');
    const key = canonical(request);
    const available = this.byDispatch.slice(this.nextDispatch).filter(event => canonical(event.request) === key).length;
    const queued = this.pending.filter(item => item.key === key).length;
    if (queued >= available) {
      throw new Error('Native tool replay has no matching remaining tool request');
    }
    let item, assign, rejectAssignment;
    const assignment = new Promise((resolve, reject) => { assign = resolve; rejectAssignment = reject; });
    // One-step consumers do not inspect assignments; cancellation is still
    // observed even when no caller requests this early view.
    assignment.catch(() => {});
    const outcome = new Promise((resolve, reject) => {
      item = { key, resolve, reject, assign, rejectAssignment, released: false };
      this.pending.push(item);
    });
    this.drain().catch(error => this.abort(error));
    return { outcome, assignment, release: () => {
      if (this.closed || item.released) throw new Error('Native tool replay reservation is unavailable');
      item.released = true;
      this.drain().catch(error => this.abort(error));
      return outcome;
    }};
  }
  invoke(request) {
    try { return this.reserve(request).release(); }
    catch (error) { return Promise.reject(error); }
  }
  async drain() {
    if (this.draining || this.closed) return;
    this.draining = true;
    try {
      while (!this.closed) {
        while (this.nextDispatch < this.byDispatch.length) {
          const expected = this.byDispatch[this.nextDispatch];
          const index = this.pending.findIndex(item => item.key === canonical(expected.request));
          if (index < 0) break;
          const item = this.pending.splice(index, 1)[0];
          this.assigned.set(expected.ordinal, item);
          this.nextDispatch++;
          item.assign(clone(expected.outcome));
        }
        const event = this.events[this.nextCompletion];
        const item = event && this.assigned.get(event.ordinal);
        if (!item || !item.released) break;
        await this.applyEffects(clone(event));
        this.assigned.delete(event.ordinal);
        this.nextCompletion++;
        if (event.outcome.ok) item.resolve(clone(event.outcome.value));
        else if (Object.hasOwn(event.outcome, 'thrown')) item.reject(decodeThrown(event.outcome.thrown));
        else { const error = new Error(event.outcome.message); error.name = event.outcome.name; item.reject(error); }
      }
    } finally { this.draining = false; }
  }
  abort(error = new Error('Native tool replay was cancelled')) {
    this.closed = true;
    for (const item of [...this.pending, ...this.assigned.values()]) { item.rejectAssignment(error); item.reject(error); }
    this.pending.length = 0;
    this.assigned.clear();
  }
  finish() {
    if (this.closed || this.pending.length || this.assigned.size || this.nextCompletion !== this.events.length) {
      throw new Error('Native tool replay left tool calls incomplete');
    }
    this.closed = true;
  }
}
