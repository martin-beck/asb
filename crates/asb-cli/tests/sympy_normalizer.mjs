import test from "node:test";
import assert from "node:assert/strict";
import { normalizeToolResult } from "../assets/examples/normalizers/sympy-test-banner.mjs";

test("only a complete SymPy banner from a matching bash command loses its seed", () => {
  const text = "==== test process starts ====\nrandom seed: 123\n==== tests finished: 1 passed ====\n";
  const tool = { name: "bash", arguments: { command: "python -c 'sympy.test(...)'" } };
  const normalized = normalizeToolResult({ text, tool });
  assert.equal(normalized, text.replace("123", "<asb-seed>"));
  assert.equal(normalizeToolResult({ text: normalized, tool }), normalized);
  assert.equal(normalizeToolResult({ text, tool: null }), text);
  assert.equal(normalizeToolResult({ text, tool: { ...tool, name: "read" } }), text);
  assert.equal(normalizeToolResult({ text, tool: { ...tool, arguments: { command: "echo seed" } } }), text);
  assert.equal(normalizeToolResult({ text: text.replace("tests finished", "truncated"), tool }), text.replace("tests finished", "truncated"));
  assert.equal(normalizeToolResult({ text: text + text, tool }), text + text);
  const changed = text.replace("1 passed", "1 failed");
  assert.notEqual(normalizeToolResult({ text: changed, tool }), normalized);
});
