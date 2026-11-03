// Real embedded sidecar qualification against bounded loopback HTTP fixtures.
import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";
import { spawn, execFileSync } from "node:child_process";
import http from "node:http";
import net from "node:net";
import { setTimeout as delay } from "node:timers/promises";

async function fixture(t, upstreamHandler, options = {}) {
  const parent = join(process.cwd(), "target/capture-http-fixtures");
  fs.mkdirSync(parent, { recursive: true });
  const root = fs.mkdtempSync(join(parent, "capture-"));
  const upstream = http.createServer(upstreamHandler);
  await new Promise((resolve) => upstream.listen(0, "127.0.0.1", resolve));
  const ready = join(root, "ready.json");
  const output = join(root, "trace.jsonl");
  const receipt = join(root, "receipt.json");
  const curlMarker = join(root, "curl.pid");
  let path = process.env.PATH;
  if (options.curl) {
    const quote = (value) => `'${value.replaceAll("'", "'\\''")}'`;
    const bin = join(root, "bin");
    fs.mkdirSync(bin);
    const script = options.resistant
      ? `#!/bin/sh\nexec ${quote(process.execPath)} -e ${quote('process.on("SIGTERM",()=>{}); require("fs").writeFileSync(process.argv[1],String(process.pid)); setInterval(()=>{},1000);')} ${quote(curlMarker)}\n`
      : `#!/bin/sh\nprintf '%s' "$$" > ${quote(curlMarker)}\nexec ${quote(execFileSync("which", ["curl"], { encoding: "utf8" }).trim())} "$@"\n`;
    fs.writeFileSync(join(bin, "curl"), script, { mode: 0o700 });
    path = `${bin}:${path}`;
  }
  const tapeScript = fileURLToPath(new URL("../assets/scripts/llmtape.mjs", import.meta.url));
  const arguments_ = [
    tapeScript,
    "record", "--port", "0", "--upstream", `http://127.0.0.1:${upstream.address().port}`, "--out", output,
  ];
  const child = spawn(process.execPath, arguments_, {
    env: { PATH: path, HOME: root, ASB_TAPE_OWNER_PIPE: "1", ASB_TAPE_READY_FILE: ready,
      ...(options.curl ? { http_proxy: "http://127.0.0.1:1", no_proxy: "127.0.0.1" } : {}),
      ASB_TAPE_CAPTURE_RECEIPT: receipt, ASB_TAPE_CAPTURE_NONCE: "fixture-private-invocation" },
    stdio: ["pipe", "ignore", "pipe"],
  });
  let stderr = "";
  child.stderr.on("data", (data) => { stderr = (stderr + data).slice(-4096); });
  const exited = new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", (code, signal) => resolve({ code, signal }));
  });
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
    await exited;
    upstream.closeAllConnections();
    await new Promise((resolve) => upstream.close(resolve));
    fs.rmSync(root, { recursive: true });
  });
  const deadline = performance.now() + 2000;
  while (!fs.existsSync(ready)) {
    assert.equal(child.exitCode, null, stderr);
    assert.ok(performance.now() < deadline, `readiness timeout: ${stderr}`);
    await delay(10);
  }
  const port = JSON.parse(fs.readFileSync(ready)).port;
  async function stop() {
    child.stdin.end();
    const result = await Promise.race([
      exited,
      delay(2500, undefined, { ref: false }).then(() => { throw new Error(`sidecar shutdown timeout: ${stderr}`); }),
    ]);
    assert.deepEqual(result, { code: 0, signal: null }, stderr);
    const value = JSON.parse(fs.readFileSync(receipt));
    const bytes = fs.readFileSync(output);
    assert.equal(value.pid, child.pid);
    assert.equal(value.nonce, "fixture-private-invocation");
    assert.equal(value.bytes, bytes.length);
    assert.equal(value.sha256, createHash("sha256").update(bytes).digest("hex"));
    assert.equal(value.inflight, 0);
    assert.equal(value.accepted, value.completed + value.failed);
    return { value, bytes };
  }
  return { port, stop, child, curlMarker, output };
}

test("oversized upload returns 413 but cannot become a complete recording", { timeout: 10000 }, async (t) => {
  const { port, stop } = await fixture(t, () => assert.fail("oversized upload reached upstream"));
  const result = await request(port, "/v1/responses", "x".repeat(16 * 1024 * 1024 + 1));
  assert.equal(result.status, 413);
  const { value } = await stop();
  assert.equal(value.accepted, 1);
  assert.equal(value.failed, 1);
  assert.equal(value.recorded, 0);
  assert.equal(value.complete, false);
});

test("large non-path response normalization stays bounded and captures a full trace", { timeout: 10000 }, async (t) => {
  const { port, stop } = await fixture(t, (_req, res) => res.end("x".repeat(12 * 1024 * 1024)));
  const result = await request(port);
  assert.equal(result.body.length, 12 * 1024 * 1024);
  const { value } = await stop();
  assert.equal(value.accepted, 1);
  assert.equal(value.recorded, 1);
  assert.ok(value.bytes > 12 * 1024 * 1024);
  assert.equal(value.failed, 0);
  assert.equal(value.complete, true);
});

for (const resistant of [false, true]) {
  test(`curl backend quiesces owned ${resistant ? "TERM-resistant fixture" : "actual curl"} before receipt`, { timeout: 10000, skip: process.platform !== "linux" }, async (t) => {
    let announce;
    const upstreamSeen = new Promise((resolve) => { announce = resolve; });
    const { port, stop, child, curlMarker } = await fixture(t, () => announce(), { curl: true, resistant });
    const sentinel = spawn(process.execPath, ["-e", "setInterval(()=>{},1000)"], { stdio: "ignore" });
    const sentinelExit = new Promise((resolve) => sentinel.once("exit", resolve));
    t.after(async () => { sentinel.kill("SIGKILL"); await sentinelExit; });
    const req = http.request({ host: "127.0.0.1", port, path: "/v1/responses", method: "POST" });
    req.on("error", () => {});
    req.on("response", (res) => { res.on("error", () => {}); res.resume(); });
    req.end("{}");
    const deadline = performance.now() + 2000;
    while (!fs.existsSync(curlMarker)) {
      assert.ok(performance.now() < deadline, "owned curl fixture never started");
      await delay(10);
    }
    if (!resistant) await upstreamSeen;
    const owned = Number(fs.readFileSync(curlMarker, "utf8"));
    assert.ok(Number.isSafeInteger(owned) && owned > 1);
    assert.ok(fs.existsSync(`/proc/${owned}`), "actual owned child must be live before stop");
    const stopped = stop();
    await delay(10);
    child.kill("SIGTERM"); // Repeated shutdown during drain must not publish twice.
    const { value } = await stopped;
    assert.equal(value.accepted, 1);
    assert.equal(value.failed, 1);
    assert.equal(value.complete, false);
    assert.equal(fs.existsSync(`/proc/${owned}`), false, "owned outbound child must actually be reaped");
    assert.equal(sentinel.exitCode, null, "unrelated process must survive outbound cleanup");
  });
}

function request(port, path = "/v1/responses", body = "{}") {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: "127.0.0.1", port, path, method: body === null ? "GET" : "POST" }, (res) => {
      const chunks = [];
      res.on("data", (chunk) => chunks.push(chunk));
      res.once("error", reject);
      res.once("end", () => resolve({ status: res.statusCode, body: Buffer.concat(chunks).toString() }));
    });
    req.once("error", reject);
    req.end(body);
  });
}

test("linear path normalization preserves legacy cassette keys across path boundaries", { timeout: 10000 }, async (t) => {
  const cases = [
    "/tmp/sweep/000/workspace-concurrency-3.asb-agent-runtime/a",
    "relative/workspace/file", "/tmp/workspace", "/tmp/workspaceSuffix",
    "/a//b/workspace", "foo/workspace-concurrency-1.suffix/bar/workspace-concurrency-2tail",
    "workspace-concurrency-1", "/workspace-concurrency-1",
    "x/workspace-concurrency-2/x/workspace-concurrency-3tail",
    "x/workspace-concurrency-2//y/workspace-concurrency-3tail",
    "/a/workspace/file /b/workspace/after", "/a/é/workspace",
    "a/workspace\\literal", "a/workspace\nnext",
  ];
  let index = 0;
  const { port, stop } = await fixture(t, (_req, res) => {
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify({ message: cases[index++] }));
  });
  for (const _ of cases) assert.equal((await request(port)).status, 200);
  const { value, bytes } = await stop();
  assert.equal(value.complete, true);
  const entries = bytes.toString().trim().split("\n").map(JSON.parse);
  cases.forEach((sample, offset) => {
    const legacy = JSON.stringify({ message: sample })
      .replace(/\/?(?:[A-Za-z0-9_.-]+\/)+workspace-concurrency-\d+(?:\.asb-agent-runtime)?/g, "<asb-workspace>")
      .replace(/\/?(?:[A-Za-z0-9_.-]+\/)+workspace(?=\/|["\\])/g, "<asb-workspace>");
    assert.deepEqual(JSON.parse(entries[offset].response.body), JSON.parse(legacy), sample);
  });
});

test("empty and probe-only captures have honest complete/zero-inference observations", { timeout: 10000 }, async (t) => {
  for (const probe of [false, true]) {
    const { port, stop } = await fixture(t, (_req, res) => res.end("{}"));
    if (probe) assert.equal((await request(port, "/health", null)).status, 200);
    const { value } = await stop();
    assert.equal(value.complete, true);
    assert.equal(value.accepted, Number(probe));
    assert.equal(value.inference_requests, 0);
    assert.equal(value.recorded, Number(probe));
  }
});

test("embedding exchanges are retained without being counted as language-model inference", { timeout: 10000 }, async (t) => {
  const { port, stop } = await fixture(t, (_req, res) => res.end("{}"));
  assert.equal((await request(port, "/v1/embeddings?fixture=1", JSON.stringify({ model: "text-embedding-3-small", input: ["memory"] }))).status, 200);
  assert.equal((await request(port, "/v1/embeddings/", JSON.stringify({ model: "fixture-embedding", input: ["memory"] }))).status, 200);
  assert.equal((await request(port, "/v1/messages", JSON.stringify({ model: "fixture", messages: [] }))).status, 200);
  const { value, bytes } = await stop();
  assert.equal(value.complete, true);
  assert.equal(value.recorded, 3);
  assert.equal(value.inference_requests, 1);
  const entries = bytes.toString().trim().split("\n").map(JSON.parse);
  assert.equal(entries[0].request.url, "/v1/embeddings?fixture=1");
  assert.equal(entries[1].request.url, "/v1/embeddings/");
  assert.equal(entries[2].request.url, "/v1/messages");
});

test("complete HTTP error is captured and grace drains concurrent already-admitted responses", { timeout: 10000 }, async (t) => {
  let seen = 0;
  let announce;
  const admitted = new Promise((resolve) => { announce = resolve; });
  const { port, stop } = await fixture(t, (_req, res) => {
    if (++seen === 2) announce();
    setTimeout(() => { res.writeHead(500, { "content-type": "application/json" }); res.end('{"error":"fixture"}'); }, 80);
  });
  const first = request(port);
  const second = request(port);
  await admitted;
  const stopped = stop();
  assert.equal((await first).status, 500);
  assert.equal((await second).status, 500);
  const { value, bytes } = await stopped;
  assert.equal(value.complete, true);
  assert.equal(value.completed, 2);
  assert.equal(value.inference_requests, 2);
  assert.equal(bytes.toString().trim().split("\n").length, 2);
});

test("partial upload is accounted before its body and forced shutdown remains bounded", { timeout: 10000 }, async (t) => {
  const { port, stop } = await fixture(t, () => assert.fail("partial upload must never reach upstream"));
  const socket = net.connect(port, "127.0.0.1");
  socket.on("error", () => {});
  t.after(() => socket.destroy());
  await new Promise((resolve) => socket.once("connect", resolve));
  socket.write("POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1000\r\n\r\nx");
  await delay(50);
  const { value } = await stop();
  assert.equal(value.accepted, 1);
  assert.equal(value.failed, 1);
  assert.equal(value.recorded, 0);
  assert.equal(value.complete, false);
});

for (const scenario of ["aborted-upstream", "hanging-upstream", "disconnected-downstream"]) {
  test(`transport interruption: ${scenario}`, { timeout: 10000 }, async (t) => {
    let announce;
    const admitted = new Promise((resolve) => { announce = resolve; });
    const { port, stop } = await fixture(t, (_req, res) => {
      announce();
      if (scenario === "aborted-upstream") {
        res.writeHead(200, { "content-length": "1000" });
        res.write("short");
        res.destroy();
      } else if (scenario === "disconnected-downstream") {
        setTimeout(() => res.end("full upstream response"), 60);
      }
    });
    const req = http.request({ host: "127.0.0.1", port, path: "/v1/responses", method: "POST" });
    req.on("error", () => {});
    req.on("response", (res) => { res.on("error", () => {}); res.resume(); });
    req.end("{}");
    await admitted;
    if (scenario === "disconnected-downstream") req.destroy();
    const { value } = await stop();
    assert.equal(value.accepted, 1);
    assert.equal(value.failed, 1);
    assert.equal(value.complete, false);
    assert.equal(value.recorded, scenario === "disconnected-downstream" ? 1 : 0);
  });
}
