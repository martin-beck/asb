// Private, content-free observation for the embedded llmtape proxy.
//
// The descriptor is supplied only by ASB's Perfetto collector.  This module
// deliberately degrades to a no-op when it is absent, so recording/replay
// semantics never depend on tracing availability.

import { closeSync, writeFileSync, writeSync } from "node:fs";

const MAX_ACTIVE = 1024;
const MAX_ERRORS = 16;

function privateFd(value) {
  if (!/^\d+$/.test(value || "")) return undefined;
  const fd = Number(value);
  return Number.isSafeInteger(fd) && fd >= 3 ? fd : undefined;
}

function fixed(value, fallback) {
  return /^[a-z0-9][a-z0-9_.-]{0,63}$/.test(value || "") ? value : fallback;
}

export class RequestTracing {
  #fd = privateFd(process.env.ASB_TAPE_PERFETTO_OBSERVER_FD);
  #report = process.env.ASB_TAPE_PERFETTO_REQUEST_REPORT;
  #traceId = fixed(process.env.ASB_TAPE_PERFETTO_TRACE_ID, "unavailable");
  #next = 0;
  #active = new Set();
  #begun = 0;
  #ended = 0;
  #instant = 0;
  #limitReached = false;
  #writeErrors = [];
  #closed = false;

  enabled() { return this.#fd !== undefined; }

  begin() {
    if (!this.enabled() || this.#closed) return undefined;
    if (this.#active.size >= MAX_ACTIVE) {
      this.#limitReached = true;
      return undefined;
    }
    const cookie = ++this.#next;
    if (this.#write(`begin llm_request ${cookie}`)) {
      this.#active.add(cookie);
      this.#begun += 1;
      return cookie;
    }
    return undefined;
  }

  end(cookie) {
    if (!this.#active.delete(cookie)) return;
    if (this.#write(`end llm_request ${cookie}`)) this.#ended += 1;
  }

  failure(cookie, reason) {
    if (cookie === undefined) return;
    const category = fixed(reason, "error");
    if (this.#write(`instant llm_request ${cookie} ${category}`)) this.#instant += 1;
  }

  // Called exactly once during proxy shutdown. It never throws: the proxy's
  // HTTP and cassette completion contracts are independent of observation.
  close() {
    if (this.#closed) return;
    this.#closed = true;
    if (this.#fd !== undefined) {
      try { closeSync(this.#fd); } catch { this.#remember("close"); }
      this.#fd = undefined;
    }
    if (!this.#report || !this.#report.startsWith("/")) return;
    const report = JSON.stringify({
      schema_version: 1,
      trace_id: this.#traceId,
      producer: "llmtape",
      begun: this.#begun,
      ended: this.#ended,
      unfinished: this.#active.size,
      complete: this.#active.size === 0 && !this.#limitReached && this.#writeErrors.length === 0,
      limit_reached: this.#limitReached,
      write_errors: this.#writeErrors,
    });
    try { writeFileSync(this.#report, report, { encoding: "utf8", mode: 0o600, flag: "wx" }); }
    catch { /* evidence is best effort and must not affect tape behavior */ }
  }

  #write(message) {
    try {
      // The relay accepts only this fixed grammar. No request-specific value is
      // ever copied into a marker, even on a failure path.
      writeSync(this.#fd, `${message}\n`, undefined, "utf8");
      return true;
    } catch {
      this.#remember("write");
      return false;
    }
  }

  #remember(kind) {
    if (this.#writeErrors.length < MAX_ERRORS && !this.#writeErrors.includes(kind)) {
      this.#writeErrors.push(kind);
    }
  }
}
