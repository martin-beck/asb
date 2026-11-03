#!/usr/bin/env node

import assert from "node:assert/strict";
import { closeSync, mkdtempSync, openSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const root = mkdtempSync(join(tmpdir(), "asb-request-tracing-"));
try {
  const marker = join(root, "observer");
  const report = join(root, "perfetto-requests.json");
  const fd = openSync(marker, "w");
  process.env.ASB_TAPE_PERFETTO_OBSERVER_FD = String(fd);
  process.env.ASB_TAPE_PERFETTO_REQUEST_REPORT = report;
  process.env.ASB_TAPE_PERFETTO_TRACE_ID = "42-0";
  const { RequestTracing } = await import("../assets/scripts/request-tracing.mjs");
  const tracing = new RequestTracing();
  const first = tracing.begin();
  const second = tracing.begin();
  tracing.failure(second, "http_error");
  tracing.end(second);
  tracing.end(first);
  tracing.close();
  const events = readFileSync(marker, "utf8").trim().split("\n");
  assert.deepEqual(events, [
    "begin llm_request 1",
    "begin llm_request 2",
    "instant llm_request 2 http_error",
    "end llm_request 2",
    "end llm_request 1",
  ]);
  const evidence = JSON.parse(readFileSync(report, "utf8"));
  assert.equal(evidence.trace_id, "42-0");
  assert.equal(evidence.complete, true);
  assert.equal(evidence.begun, 2);
  assert.equal(evidence.ended, 2);
  // `close` owns the descriptor after the tracing object is constructed.
  assert.throws(() => closeSync(fd));
} finally {
  rmSync(root, { recursive: true, force: true });
}
