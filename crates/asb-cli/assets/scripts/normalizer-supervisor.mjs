import { Worker } from "node:worker_threads";
import { createHash } from "node:crypto";

// One worker per proxy session, with deadlines enforced independently of the
// worker's event loop. It is intentionally not advertised as a sandbox.
export class NormalizerSupervisor {
  constructor(profiles, { callTimeoutMs = 1000, startupTimeoutMs = 5000, maxOutputBytes = 16 * 1024 * 1024, maxPending = 64 } = {}) {
    this.pending = new Map();
    this.nextId = 0;
    this.closed = false;
    // Each profile has its own VM deadline; the parent must allow for all
    // profiles and normal worker scheduling latency.
    this.requestTimeoutMs = callTimeoutMs * Math.max(1, profiles.length) + 2000;
    this.maxPending = maxPending;
    this.counts = new Map(profiles.map(({ id }) => [id, { observations: 0, changed: 0 }]));
    this.changes = [];
    this.worker = new Worker(new URL("./normalizer-worker.mjs", import.meta.url), {
      workerData: { profiles, callTimeoutMs, maxOutputBytes },
      resourceLimits: { maxOldGenerationSizeMb: 64 },
    });
    this.ready = new Promise((resolve, reject) => {
      this.startup = { resolve, reject, timer: setTimeout(() => this.fail(new Error("normalizer worker startup timed out")), startupTimeoutMs) };
    });
    this.worker.on("message", (message) => this.onMessage(message));
    this.worker.on("error", (error) => this.fail(error));
    this.worker.on("exit", (code) => { if (!this.closed) this.fail(new Error(`normalizer worker exited (${code})`)); });
  }

  onMessage(message) {
    if (message.type === "ready" && this.startup) {
      clearTimeout(this.startup.timer); this.startup.resolve(); this.startup = null; return;
    }
    if (message.type === "failure" && message.id === undefined) { this.fail(new Error(message.message)); return; }
    const request = this.pending.get(message.id);
    if (!request) return;
    clearTimeout(request.timer); this.pending.delete(message.id);
    if (message.type === "result") {
      const changed = new Set(message.changed);
      for (const [id, counts] of this.counts) {
        counts.observations += 1;
        if (changed.has(id)) counts.changed += 1;
      }
      if (message.changed.length && this.changes.length < 64) {
        const digest = (value) => createHash("sha256").update(value, "utf8").digest("hex");
        this.changes.push({ profiles: message.changed, before_sha256: request.before, after_sha256: digest(message.text) });
      }
      request.resolve(message.text);
    }
    else this.fail(new Error(message.message || "normalizer worker failed"), request);
  }

  // Observations count visits, including repeated conversation history; they
  // are not presented as distinct tool calls or distinct results.
  diagnostics() {
    return [...this.counts].map(([id, counts]) => ({ id, ...counts }));
  }

  changedResultDigests() { return this.changes.map((entry) => ({ ...entry, profiles: [...entry.profiles] })); }

  fail(error, current) {
    if (this.closed) return;
    this.closed = true;
    if (this.startup) { clearTimeout(this.startup.timer); this.startup.reject(error); this.startup = null; }
    if (current) current.reject(error);
    for (const request of this.pending.values()) { clearTimeout(request.timer); request.reject(error); }
    this.pending.clear();
    void this.worker.terminate();
  }

  async normalize(context) {
    await this.ready;
    if (this.closed) throw new Error("normalizer worker is closed");
    if (this.pending.size >= this.maxPending) throw new Error("normalizer worker queue is full");
    const id = ++this.nextId;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => this.fail(new Error("normalizer execution timed out")), this.requestTimeoutMs);
      this.pending.set(id, { resolve, reject, timer, before: createHash("sha256").update(context.text, "utf8").digest("hex") });
      this.worker.postMessage({ id, context });
    });
  }

  async close() {
    if (this.closed) return;
    this.fail(new Error("normalizer worker closed"));
    await this.worker.terminate();
  }
}
