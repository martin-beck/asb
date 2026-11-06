import test from "node:test";
import assert from "node:assert/strict";
import { normalizeToolResult as unittest } from "../assets/examples/normalizers/python-unittest-elapsed.mjs";
import { normalizeToolResult as diff } from "../assets/examples/normalizers/unified-diff-mtime.mjs";
import { normalizeToolResult as pytest } from "../assets/examples/normalizers/pytest-elapsed.mjs";
import { normalizeToolResult as search } from "../assets/examples/normalizers/opencode-search-order.mjs";
import { normalizeToolResult as ls } from "../assets/examples/normalizers/shell-ls-diagnostics.mjs";
import { normalizeToolResult as repr } from "../assets/examples/normalizers/python-repr-address.mjs";
import { normalizeToolResult as pip } from "../assets/examples/normalizers/pip-transfer-progress.mjs";
import { normalizeToolResult as grepOrder } from "../assets/examples/normalizers/grep-recursive-order.mjs";

test("recursive grep ordering is normalized only for complete unambiguous results", () => {
  const tool = {name:'Bash', arguments:{command:'grep -rn "_latex" sympy/vector/ | head -50'}};
  const a = 'sympy/vector/z.py:2: foo\nsympy/vector/a.py:3: bar\n';
  const b = 'sympy/vector/a.py:3: bar\nsympy/vector/z.py:2: foo\n';
  assert.equal(grepOrder({text:a,tool}), grepOrder({text:b,tool}));
  assert.notEqual(grepOrder({text:a.replace('foo','changed'),tool}), grepOrder({text:b,tool}));
  const incomplete = a.slice(0, -1);
  assert.equal(grepOrder({text:incomplete,tool}), grepOrder({text:b.slice(0, -1),tool}));
  assert.notEqual(grepOrder({text:incomplete,tool}), grepOrder({text:b,tool}));
  assert.equal(grepOrder({text:incomplete,tool:{...tool,name:'bash'}}), incomplete);
  const joined = 'sympy/vector/a.py:3: barsympy/vector/z.py:2: foo\n';
  assert.equal(grepOrder({text:joined,tool}), joined);
  const truncated = {...tool, arguments:{command:'grep -rn "_latex" sympy/vector/ | head -2'}};
  assert.equal(grepOrder({text:a,tool:truncated}), a);
  const unrelated = {...tool, arguments:{command:'cat log'}};
  assert.equal(grepOrder({text:a,tool:unrelated}), a);
  const multiple = {...tool, arguments:{command:'grep -rn "BaseScalar" sympy/printing/*.py sympy/vector/*.py | head -50'}};
  const first = 'sympy/vector/z.py:2: foo\nsympy/printing/a.py:3: bar';
  const second = 'sympy/printing/a.py:3: bar\nsympy/vector/z.py:2: foo';
  assert.equal(grepOrder({text:first,tool:multiple}), grepOrder({text:second,tool:multiple}));
  const foreign = first.replace('sympy/vector/z.py','outside/z.py');
  assert.equal(grepOrder({text:foreign,tool:multiple}), foreign);
});

test("unittest elapsed normalization requires the command and a complete summary", () => {
  const tool = { name: "bash", arguments: { command: "python3 -m unittest tests" } };
  const text = "Ran 3 tests in 1.25s\n\nOK\n";
  const expected = "Ran 3 tests in <asb-elapsed>\n\nOK\n";
  assert.equal(unittest({ text, tool }), expected);
  assert.equal(unittest({ text: expected, tool }), expected);
  assert.equal(unittest({ text, tool: null }), text);
  assert.equal(unittest({ text, tool: { ...tool, arguments: { command: "cat report" } } }), text);
  assert.equal(unittest({ text: text.replace("OK", ""), tool }), text.replace("OK", ""));
  assert.notEqual(unittest({ text: text.replace("3 tests", "4 tests"), tool }), expected);
});

test("pytest normalization is confined to associated pytest output", () => {
  const tool = { name: "bash", arguments: { command: "python -m pytest tests" } };
  const text = "1 passed, 2 skipped in 3.45s\n";
  const expected = "1 passed, 2 skipped in <asb-elapsed>\n";
  assert.equal(pytest({ text, tool }), expected);
  assert.equal(pytest({ text: expected, tool }), expected);
  assert.equal(pytest({ text, tool: null }), text);
  assert.equal(pytest({ text, tool: { ...tool, arguments: { command: "cat test.log" } } }), text);
  assert.notEqual(pytest({ text: text.replace("1 passed", "2 passed"), tool }), expected);
  const warnings = 'FAILED test_x.py::test_one\n3 failed, 127 passed, 3 warnings in 4.58s\n';
  const normalized = pytest({text:warnings,tool});
  assert.equal(pytest({text:warnings.replace('4.58s','4.51s'),tool}), normalized);
  assert.notEqual(pytest({text:warnings.replace('3 warnings','2 warnings'),tool}), normalized);
  assert.notEqual(pytest({text:warnings.replace('test_one','test_other'),tool}), normalized);
});

test("diff normalization keeps paths, hunks and unrelated command output", () => {
  const tool = { name: "bash", arguments: { command: "diff -u before after" } };
  const text = "--- before\t2026-01-01 10:10:10 +0000\n+++ after\t2026-01-01 10:10:11 +0000\n@@ -1 +1 @@\n-old\n+new\n";
  const expected = text.replace(/2026-01-01 10:10:1[01] \+0000/g, "<asb-diff-mtime>");
  assert.equal(diff({ text, tool }), expected);
  assert.equal(diff({ text: expected, tool }), expected);
  assert.equal(diff({ text, tool: null }), text);
  assert.equal(diff({ text, tool: { ...tool, arguments: { command: "cat patch" } } }), text);
  assert.notEqual(diff({ text: text.replace("+new", "+other"), tool }), expected);
});

test("search ordering requires an associated grep and preserves match content", () => {
  const text = "Found 2 matches\n/testbed/z:1: z\n\n/testbed/a:2: a\n";
  const expected = "Found 2 matches\n/testbed/a:2: a\n\n/testbed/z:1: z\n";
  assert.equal(search({ text, tool: { name: "grep" } }), expected);
  assert.equal(search({ text: expected, tool: { name: "grep" } }), expected);
  assert.equal(search({ text, tool: { name: "bash" } }), text);
  assert.notEqual(search({ text: text.replace(":2: a", ":2: changed"), tool: { name: "grep" } }), expected);
});

test("shell, Python and pip rules require the matching associated command", () => {
  const samples = [
    [ls, "ls missing .", "entry\nls: cannot access 'missing': No such file\n", "ls: cannot access 'missing': No such file\nentry\n"],
    [repr, "python3 script.py", "<Foo object at 0x123> <function f at 0xabc>", "<Foo object at <asb-address>> <function f at <asb-address>>"],
    [pip, "python -m pip install package", "2/3 MB 4 MB/s eta 0:00:01", "2/3 MB <asb-transfer-rate> eta <asb-transfer-eta>"],
  ];
  for (const [normalize, command, text, expected] of samples) {
    const tool = { name: "bash", arguments: { command } };
    assert.equal(normalize({ text, tool }), expected);
    assert.equal(normalize({ text: expected, tool }), expected);
    assert.equal(normalize({ text, tool: null }), text);
    assert.equal(normalize({ text, tool: { name: "bash", arguments: { command: "cat output" } } }), text);
  }
});
