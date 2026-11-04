// Continue-specific native dispatch ordering. Storage and cross-process routing
// are supplied by the caller; this module never starts a tool during replay.
function canonical(value) {
  if (value === null || typeof value === 'boolean' || typeof value === 'string') return JSON.stringify(value);
  if (typeof value === 'number' && Number.isFinite(value)) return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (typeof value === 'object' && Object.getPrototypeOf(value) === Object.prototype) {
    return `{${Object.keys(value).sort().map(key => `${JSON.stringify(key)}:${canonical(value[key])}`).join(',')}}`;
  }
  throw new TypeError('Continue tool exchange is not representable as JSON');
}
function clone(value) { return JSON.parse(canonical(value)); }
export function normalizeRequest(value) { return clone(value); }

export class RecordingSession {
  constructor() { this.next = 0; this.completed = []; this.active = new Set(); this.closed = false; this.invalid = false; }
  async invoke(request, execute) {
    if (this.closed || typeof execute !== 'function') throw new Error('Continue recording session is unavailable');
    const normalized = clone(request);
    const ordinal = this.next++;
    this.active.add(ordinal);
    let value;
    try {
      value = await execute();
    } catch (error) {
      if (!(error instanceof Error)) { this.invalid = true; throw error; }
      this.completed.push({ ordinal, request: normalized, outcome: { ok: false, name: error.name, message: error.message } });
      throw error;
    } finally { this.active.delete(ordinal); }
    try {
      this.completed.push({ ordinal, request: normalized, outcome: { ok: true, value: clone(value) } });
    } catch (error) { this.invalid = true; throw error; }
    return value;
  }
  preview() {
    if (this.closed || this.invalid || this.active.size) throw new Error('Continue recording has unfinished tool calls');
    return clone(this.completed);
  }
  finish() { const events = this.preview(); this.closed = true; return events; }
  abort() { this.invalid = true; this.closed = true; }
}

export class ReplaySession {
  constructor(events, applyEffects = async () => {}) {
    this.events = clone(events);
    if (!Array.isArray(this.events) || typeof applyEffects !== 'function') throw new TypeError('Continue replay input is invalid');
    this.byDispatch = [...this.events].sort((a, b) => a.ordinal - b.ordinal);
    if (this.byDispatch.some((event, index) => event.ordinal !== index || !event.request || !event.outcome ||
        typeof event.outcome.ok !== 'boolean' ||
        (event.outcome.ok && !Object.hasOwn(event.outcome, 'value')) ||
        (!event.outcome.ok && (typeof event.outcome.name !== 'string' || typeof event.outcome.message !== 'string')))) {
      throw new Error('Continue replay events are incomplete');
    }
    this.applyEffects = applyEffects;
    this.nextDispatch = 0;
    this.nextCompletion = 0;
    this.pending = [];
    this.assigned = new Map();
    this.draining = false;
    this.closed = false;
  }
  invoke(request) {
    if (this.closed) return Promise.reject(new Error('Continue replay session is closed'));
    const key = canonical(request);
    if (!this.byDispatch.slice(this.nextDispatch).some(event => canonical(event.request) === key)) {
      return Promise.reject(new Error('Continue replay has no matching remaining tool request'));
    }
    return new Promise((resolve, reject) => {
      this.pending.push({ key, resolve, reject });
      this.drain().catch(error => this.abort(error));
    });
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
          this.assigned.set(expected.ordinal, this.pending.splice(index, 1)[0]);
          this.nextDispatch++;
        }
        const event = this.events[this.nextCompletion];
        const item = event && this.assigned.get(event.ordinal);
        if (!item) break;
        await this.applyEffects(clone(event));
        this.assigned.delete(event.ordinal);
        this.nextCompletion++;
        if (event.outcome.ok) item.resolve(clone(event.outcome.value));
        else { const error = new Error(event.outcome.message); error.name = event.outcome.name; item.reject(error); }
      }
    } finally { this.draining = false; }
  }
  abort(error = new Error('Continue replay was cancelled')) {
    this.closed = true;
    for (const item of [...this.pending, ...this.assigned.values()]) item.reject(error);
    this.pending.length = 0;
    this.assigned.clear();
  }
  finish() {
    if (this.closed || this.pending.length || this.assigned.size || this.nextCompletion !== this.events.length) {
      throw new Error('Continue replay left tool calls incomplete');
    }
    this.closed = true;
  }
}
