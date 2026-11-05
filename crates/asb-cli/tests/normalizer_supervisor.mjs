import test from "node:test";
import assert from "node:assert/strict";
import { NormalizerSupervisor } from "../assets/scripts/normalizer-supervisor.mjs";

test("worker applies selected profiles in order without mutating input", async () => {
  const supervisor = new NormalizerSupervisor([
    { id: "first", source: "export function normalizeToolResult({text}) { return text.replace('1', '2'); }" },
    { id: "second", source: "export function normalizeToolResult({text, tool}) { return text + ':' + tool.name; }" },
  ]);
  const context = { text: "seed=1", tool: { name: "bash" } };
  assert.equal(await supervisor.normalize(context), "seed=2:bash");
  assert.equal(context.text, "seed=1");
  assert.equal(await supervisor.normalize({ text: "unchanged", tool: { name: "bash" } }), "unchanged:bash");
  assert.deepEqual(supervisor.diagnostics(), [
    { id: "first", observations: 2, changed: 1 },
    { id: "second", observations: 2, changed: 2 },
  ]);
  await supervisor.close();
});

test("bad modules, exceptions, promises and infinite loops fail", async () => {
  for (const source of [
    "export function wrong() {}",
    "import x from 'x'; export function normalizeToolResult({text}) { return text; }",
    "export function normalizeToolResult() { throw new Error('failure'); }",
    "export function normalizeToolResult() { return Promise.resolve('bad'); }",
    "export function normalizeToolResult() { while (true) {} }",
  ]) {
    const supervisor = new NormalizerSupervisor([{ id: "bad", source }], { callTimeoutMs: 100 });
    await assert.rejects(supervisor.normalize({ text: "x", tool: null }));
    await supervisor.close();
  }
});

test("output is bounded and ambient time and randomness are absent", async () => {
  const supervisor = new NormalizerSupervisor([{ id: "large", source: "export function normalizeToolResult() { return '12345'; }" }], { maxOutputBytes: 4 });
  await assert.rejects(supervisor.normalize({ text: "x" }), /exceeded the output limit/);
  await supervisor.close();
  const deterministic = new NormalizerSupervisor([{ id: "ambient", source: "export function normalizeToolResult() { return typeof Date + ':' + typeof Math.random + ':' + typeof process; }" }]);
  assert.equal(await deterministic.normalize({ text: "x" }), "undefined:undefined:undefined");
  await deterministic.close();
});

test("module initialization errors identify the profile without leaking exception text", async () => {
  const supervisor = new NormalizerSupervisor([{
    id: "startup",
    source: "throw new Error('private fixture secret'); export function normalizeToolResult({text}) { return text; }",
  }]);
  await assert.rejects(supervisor.normalize({ text: "x" }), (error) => {
    assert.match(error.message, /normalizer startup: could not be initialized/);
    assert.doesNotMatch(error.message, /private fixture secret/);
    return true;
  });
  await supervisor.close();
});

test("changed-result digests are bounded while observation counts remain accurate", async () => {
  const supervisor = new NormalizerSupervisor([{ id: "changed", source: "export function normalizeToolResult({text}) { return text + '!'; }" }]);
  try {
    for (let i = 0; i < 70; i++) await supervisor.normalize({ text: `result-${i}` });
    assert.deepEqual(supervisor.diagnostics(), [{ id: "changed", observations: 70, changed: 70 }]);
    const digests = supervisor.changedResultDigests();
    assert.equal(digests.length, 64);
    assert.match(digests[0].before_sha256, /^[0-9a-f]{64}$/);
    assert.match(digests[0].after_sha256, /^[0-9a-f]{64}$/);
    assert.notEqual(digests[0].before_sha256, digests[0].after_sha256);
    assert.deepEqual(digests[0].profiles, ["changed"]);
    assert.doesNotMatch(JSON.stringify(digests), /result-/);
  } finally { await supervisor.close(); }
});
