import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { CaptureSession, completedSseResponse, responseDelivery } from "../assets/scripts/capture-session.mjs";
import { Writable } from "node:stream";
import { EventEmitter } from "node:events";

function fixture(t, overrides = {}) {
  const parent = join(process.cwd(), "target/capture-session-fixtures");
  fs.mkdirSync(parent, { recursive: true });
  const root = fs.mkdtempSync(join(parent, "capture-"));
  const output = join(root, "trace.jsonl");
  const receipt = join(root, "receipt.json");
  const capture = new CaptureSession(output, { pid: process.pid, nonce: "fixture-nonce" }, { ...fs, ...overrides });
  t.after(() => {
    if (capture.outputCloseError !== null) assert.throws(() => capture.close(), (error) => error === capture.outputCloseError);
    else capture.close();
    fs.rmSync(root, { recursive: true });
  });
  return { capture, output, receipt };
}

function verifyBytes(value, output) {
  const bytes = fs.readFileSync(output);
  assert.equal(value.bytes, bytes.length);
  assert.equal(value.sha256, createHash("sha256").update(bytes).digest("hex"));
  assert.equal(value.accepted, value.completed + value.failed + value.inflight);
}

test("out-of-order handlers preserve exact UTF-8 bytes and separate inference/probes", (t) => {
  const { capture, output, receipt } = fixture(t);
  const first = capture.begin();
  const second = capture.begin();
  capture.append(second, { response: "Grüße 🦀" }, true);
  capture.end(second, true);
  capture.drain();
  assert.equal(capture.begin(), null);
  assert.throws(() => capture.finalize(receipt), /not quiesced/);
  assert.equal(fs.existsSync(receipt), false);
  capture.append(first, { status: 500, probe: true }, false);
  capture.end(first, true);
  const value = capture.finalize(receipt);
  assert.equal(value.complete, true);
  assert.equal(value.inference_requests, 1);
  assert.equal(value.recorded, 2);
  assert.equal(value.nonce, "fixture-nonce");
  assert.deepEqual(JSON.parse(fs.readFileSync(receipt)), value);
  verifyBytes(value, output);
  assert.throws(() => capture.end(first, true), /not owned/);
  assert.throws(() => capture.finalize(receipt), /already attempted/);
});

for (const scenario of ["empty", "probe-only", "downstream-disconnect", "upload-aborted"]) {
  test(`terminal accounting: ${scenario}`, (t) => {
    const { capture, output, receipt } = fixture(t);
    if (scenario !== "empty") {
      const token = capture.begin();
      if (scenario !== "upload-aborted") capture.append(token, { response: "full response" }, false);
      capture.end(token, scenario === "probe-only");
    }
    const value = capture.finalize(receipt);
    assert.equal(value.complete, scenario === "empty" || scenario === "probe-only");
    assert.equal(value.inference_requests, 0);
    verifyBytes(value, output);
  });
}

test("short writes are completed and a later write failure retains honest incomplete bytes", (t) => {
  let writes = 0;
  const { capture, output, receipt } = fixture(t, {
    writeSync(fd, bytes, offset, length) {
      if (++writes === 3) throw new Error("injected disk full");
      return fs.writeSync(fd, bytes, offset, Math.min(length, 2));
    },
  });
  const token = capture.begin();
  assert.throws(() => capture.append(token, { response: "partial" }, true), /disk full/);
  capture.end(token, false);
  const value = capture.finalize(receipt);
  assert.equal(value.complete, false);
  assert.equal(value.recorded, 0);
  assert.equal(value.inference_requests, 0);
  assert.equal(value.bytes, 4);
  verifyBytes(value, output);
});

test("failed payload sync never publishes a receipt", (t) => {
  const { capture, receipt } = fixture(t, { fsyncSync() { throw new Error("injected sync failure"); } });
  assert.throws(() => capture.finalize(receipt), /sync failure/);
  assert.equal(fs.existsSync(receipt), false);
  assert.throws(() => capture.finalize(receipt), /already attempted/);
});

test("output and receipt destinations are never overwritten", (t) => {
  const { capture, output, receipt } = fixture(t);
  assert.throws(() => new CaptureSession(output, { pid: process.pid, nonce: "other" }), /EEXIST/);
  fs.writeFileSync(receipt, "preserve existing receipt");
  assert.throws(() => capture.finalize(receipt), /EEXIST/);
  assert.equal(fs.readFileSync(receipt, "utf8"), "preserve existing receipt");
});

for (const phase of ["payload-sync", "output-close", "receipt-write", "receipt-sync", "receipt-close", "link"]) {
  test(`uncertain finalization is terminal: ${phase}`, (t) => {
    let syncs = 0;
    let closes = 0;
    let injected = false;
    function fail() {
      injected = true;
      throw new Error(`injected ${phase}`);
    }
    const { capture, receipt } = fixture(t, {
      fsyncSync(fd) {
        syncs += 1;
        if (!injected && ((phase === "payload-sync" && syncs === 1) || (phase === "receipt-sync" && syncs === 2))) fail();
        return fs.fsyncSync(fd);
      },
      closeSync(fd) {
        closes += 1;
        // Model an ambiguous close that has already closed its descriptor;
        // another close must not accidentally target a subsequently reused fd.
        fs.closeSync(fd);
        if (!injected && ((phase === "output-close" && closes === 1) || (phase === "receipt-close" && closes === 2))) fail();
      },
      writeFileSync(...args) {
        if (!injected && phase === "receipt-write") fail();
        return fs.writeFileSync(...args);
      },
      linkSync(...args) {
        if (!injected && phase === "link") fail();
        return fs.linkSync(...args);
      },
    });
    assert.throws(() => capture.finalize(receipt), /injected/);
    assert.equal(injected, true);
    assert.equal(fs.existsSync(receipt), false);
    assert.throws(() => capture.finalize(receipt), /already attempted/);
    if (phase === "output-close") assert.throws(() => capture.close(), /injected output-close/);
    else capture.close();
    const before = closes;
    if (phase === "output-close") assert.throws(() => capture.close(), /injected output-close/);
    else capture.close();
    assert.equal(closes, before, "closure must be idempotent after uncertain finalization");
  });
}

test("published receipt survives temporary-link cleanup failure without republishing", (t) => {
  const { capture, output, receipt } = fixture(t, {
    unlinkSync() { throw new Error("injected cleanup failure"); },
  });
  const value = capture.finalize(receipt);
  assert.equal(value.complete, true);
  assert.equal(capture.finalized, true);
  assert.equal(capture.publicationCleanupConfirmed, false);
  assert.match(capture.publicationCleanupError.message, /cleanup failure/);
  assert.deepEqual(JSON.parse(fs.readFileSync(receipt)), value);
  assert.throws(() => capture.finalize(receipt), /already attempted/);
  verifyBytes(value, output);
});

test("accepting-stage overflow poisons completeness; draining rejection does not", (t) => {
  const { capture } = fixture(t);
  capture.accepted = 1_000_000;
  assert.throws(() => capture.begin(), /count exceeds limit/);
  assert.equal(capture.poisoned, true);
  capture.drain();
  assert.equal(capture.begin(), null);
});

test("plain close retains uncertainty without retrying a possibly reused descriptor", (t) => {
  let closes = 0;
  const { capture } = fixture(t, {
    closeSync(fd) {
      closes += 1;
      fs.closeSync(fd);
      throw new Error("ambiguous close");
    },
  });
  assert.throws(() => capture.close(), /ambiguous close/);
  assert.equal(capture.outputCleanupConfirmed, false);
  assert.throws(() => capture.close(), (error) => error === capture.outputCloseError);
  assert.equal(closes, 1);
});

test("full append with a held transport write remains pending until destroy and incomplete", async (t) => {
  const { capture, receipt } = fixture(t);
  let held;
  const response = new Writable({ write(_bytes, _encoding, callback) { held = callback; } });
  response.on("error", () => {});
  const delivered = responseDelivery(response);
  let settled = false;
  delivered.then(() => { settled = true; });
  const token = capture.begin();
  capture.append(token, { response: "fully recorded before transport handoff" }, true);
  response.end("blocked transport");
  await new Promise(setImmediate);
  assert.equal(typeof held, "function");
  assert.equal(response.writableFinished, false);
  assert.equal(settled, false);
  assert.equal(capture.pending.size, 1);
  response.destroy();
  capture.end(token, await delivered);
  const value = capture.finalize(receipt);
  assert.equal(value.recorded, 1);
  assert.equal(value.failed, 1);
  assert.equal(value.complete, false);
});

test("only finish is positive even when a detached response claims writableFinished", async () => {
  for (const terminal of ["close", "error", "finish"]) {
    const response = new EventEmitter();
    response.writableFinished = true;
    const delivered = responseDelivery(response);
    response.emit(terminal);
    response.emit("close");
    const outcome = await delivered;
    assert.equal(outcome.delivered, terminal === "finish");
    assert.equal(outcome.terminal, terminal);
  }
});

test("a close is positive only for an affirmed complete frame and clean request", async () => {
  const response = new EventEmitter();
  const delivered = responseDelivery(response, () => true);
  response.emit("close");
  assert.deepEqual(await delivered, { terminal: "framed_close", delivered: true });

  const incomplete = new EventEmitter();
  const rejected = responseDelivery(incomplete, () => false);
  incomplete.emit("close");
  assert.deepEqual(await rejected, { terminal: "close", delivered: false });
});

test("terminal SSE framing accepts horizontal whitespace but rejects partial streams", () => {
  const headers = { "content-type": "text/event-stream; charset=utf-8" };
  assert.equal(completedSseResponse(headers, "event: message_stop\ndata: {}\n"), true);
  assert.equal(completedSseResponse(headers, "data: [DONE]\n"), true);
  assert.equal(completedSseResponse(headers, "event: message_stop\ndata: {}"), false);
  assert.equal(completedSseResponse(headers, "event: message_delta\ndata: {}\n"), false);
  assert.equal(completedSseResponse({ "content-type": "application/json" }, "event: message_stop\n"), false);
});

test("incomplete records retain bounded terminal classifications without payloads", (t) => {
  const { capture, receipt } = fixture(t);
  const closed = capture.begin({ method: "POST", path: "/v1/messages" });
  capture.append(closed, { response: "recorded" }, true);
  capture.end(closed, { terminal: "close", delivered: false });
  const errored = capture.begin({ method: "POST", path: "/v1/embeddings" });
  capture.append(errored, { response: "recorded" }, false);
  capture.end(errored, { terminal: "error", delivered: false });
  const value = capture.finalize(receipt);
  assert.equal(value.complete, false);
  assert.deepEqual(value.delivery_failures, [
    { sequence: 1, method: "POST", path: "/v1/messages", cause: "close" },
    { sequence: 2, method: "POST", path: "/v1/embeddings", cause: "error" },
  ]);
});

test("complete records do not retain a delivery sidecar", (t) => {
  const { capture, receipt } = fixture(t);
  const token = capture.begin({ method: "POST", path: "/v1/messages" });
  capture.append(token, { response: "recorded" }, true);
  capture.end(token, { terminal: "finish", delivered: true });
  assert.equal(capture.finalize(receipt).complete, true);
  assert.equal(fs.existsSync(`${receipt}.delivery.json`), false);
});
