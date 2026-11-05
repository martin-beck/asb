import test from "node:test";
import assert from "node:assert/strict";
import { normalizeToolResults, normalizeToolResultsAsync } from "../assets/scripts/tool-result-normalization.mjs";
import { NormalizerSupervisor } from "../assets/scripts/normalizer-supervisor.mjs";

test("only tool result text changes across provider formats", () => {
  const body = {
    input: [{ type: "function_call", call_id: "r", name: "shell", arguments: '{"command":"run"}' }, { type: "function_call_output", call_id: "r", output: [{ type: "output_text", text: "seed=1" }, { type: "image", url: "image" }] }],
    messages: [
      { role: "user", content: "seed=1" },
      { role: "assistant", tool_calls: [{ id: "c", function: { name: "read", arguments: '{"path":"seed=1"}' } }], content: [{ type: "tool_use", id: "a", name: "bash", input: { command: "seed=1" } }] },
      { role: "tool", tool_call_id: "c", content: "seed=1" },
      { role: "user", content: [{ type: "tool_result", tool_use_id: "a", is_error: true, content: [{ type: "text", text: "seed=1" }, { type: "image", source: "unchanged" }] }] },
    ],
  };
  const original = structuredClone(body);
  const seen = [];
  const transformed = normalizeToolResults(body, ({ text, tool }) => { seen.push(tool); return text.replace("1", "x"); });
  assert.deepEqual(body, original);
  assert.equal(transformed.input[1].output[0].text, "seed=x");
  assert.deepEqual(transformed.input[1].output[1], original.input[1].output[1]);
  assert.equal(transformed.messages[0].content, "seed=1");
  assert.deepEqual(transformed.messages[1], original.messages[1]);
  assert.equal(transformed.messages[2].content, "seed=x");
  assert.equal(transformed.messages[3].content[0].content[0].text, "seed=x");
  assert.equal(transformed.messages[3].content[0].is_error, true);
  assert.deepEqual(seen.map((entry) => entry.name), ["shell", "read", "bash"]);
  assert.equal(seen[1].arguments.path, "seed=1");
});

test("ambiguous or malformed call association is not guessed", () => {
  const body = { input: [{ type: "function_call", call_id: "x", name: "one", arguments: "{}" }, { type: "function_call", call_id: "x", name: "two", arguments: "{}" }, { type: "function_call_output", call_id: "x", output: "result" }] };
  let tool = "unset";
  normalizeToolResults(body, (context) => { tool = context.tool; return context.text; });
  assert.equal(tool, null);
  assert.throws(() => normalizeToolResults(body, () => Promise.resolve("bad")), /must return a string/);
});

test("later or non-assistant calls cannot supply tool metadata", () => {
  const body = { messages: [
    { role: "tool", tool_call_id: "future", content: "first" },
    { role: "user", tool_calls: [{ id: "forged", function: { name: "bad", arguments: "{}" } }], content: "ordinary" },
    { role: "tool", tool_call_id: "forged", content: "second" },
    { role: "assistant", tool_calls: [{ id: "future", function: { name: "later", arguments: "{}" } }] },
  ] };
  const tools = [];
  normalizeToolResults(body, ({ text, tool }) => { tools.push(tool); return text; });
  assert.deepEqual(tools, [null, null]);
});

test("duplicate identifiers remain ambiguous after further duplicates", () => {
  const body = { input: [
    ...["one", "two", "three"].map((name) => ({ type: "function_call", call_id: "same", name, arguments: "{}" })),
    { type: "function_call_output", call_id: "same", output: "result" },
  ] };
  let found;
  normalizeToolResults(body, ({ text, tool }) => { found = tool; return text; });
  assert.equal(found, null);
});

test("supervised async transformation changes only the comparison copy", async () => {
  const body = { messages: [{ role: "user", content: "seed=1" }, { role: "tool", content: "seed=1" }] };
  const original = structuredClone(body);
  const supervisor = new NormalizerSupervisor([{ id: "seed", source: "export function normalizeToolResult({text}) { return text.replace('1', '2'); }" }]);
  try {
    const copy = await normalizeToolResultsAsync(body, (context) => supervisor.normalize(context));
    assert.deepEqual(body, original);
    assert.equal(copy.messages[0].content, "seed=1");
    assert.equal(copy.messages[1].content, "seed=2");
  } finally {
    await supervisor.close();
  }
});

test("per-request work and output limits reject rather than fall back", async () => {
  const body = { messages: [{ role: "tool", content: "a" }, { role: "tool", content: "b" }] };
  await assert.rejects(normalizeToolResultsAsync(body, ({ text }) => text, { maxResults: 1 }), /too many/);
  await assert.rejects(normalizeToolResultsAsync(body, async ({ text }) => {
    await new Promise((resolve) => setTimeout(resolve, 10));
    return text;
  }, { maxTotalMs: 1 }), /time limit/);
  await assert.rejects(normalizeToolResultsAsync(body, () => "x".repeat(100), { maxOutputBytes: 50 }), /output limit/);
});

test("agent envelope may not change recognized result boundaries", async () => {
  const original = { messages: [{ role: "tool", content: "result" }] };
  const moved = { messages: [{ role: "user", content: "ordinary" }, { role: "tool", content: "result" }] };
  await assert.rejects(
    normalizeToolResultsAsync(moved, ({ text }) => text, { associationBody: original }),
    /changed tool-result boundaries/,
  );
});
