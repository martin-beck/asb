import test from "node:test";
import assert from "node:assert/strict";
import { normalizeToolResult as unittest } from "../assets/examples/normalizers/python-unittest-elapsed.mjs";
import { normalizeToolResult as diff } from "../assets/examples/normalizers/unified-diff-mtime.mjs";
import { normalizeToolResult as pytest } from "../assets/examples/normalizers/pytest-elapsed.mjs";
import { normalizeToolResult as search } from "../assets/examples/normalizers/opencode-search-order.mjs";
import { normalizeToolResult as ls } from "../assets/examples/normalizers/shell-ls-diagnostics.mjs";
import { normalizeToolResult as repr } from "../assets/examples/normalizers/python-repr-address.mjs";
import { normalizeToolResult as pip } from "../assets/examples/normalizers/pip-transfer-progress.mjs";
import { NormalizerSupervisor } from "../assets/scripts/normalizer-supervisor.mjs";
import { readFileSync } from "node:fs";
import { normalizeToolResult as grepOrder } from "../assets/examples/normalizers/grep-recursive-order.mjs";
import { normalizeToolResult as tmpListing } from "../assets/examples/normalizers/tmp-listing-time.mjs";

test('temporary listings normalize only modification times', () => {
  const tool = {name:'Bash',arguments:{command:'cd /tmp && pip download sympy==1.6 --no-deps -q 2>&1 | tail -3; ls -la /tmp | head'}};
  const text = 'total 5660\ndrwxrwxrwt  2 root root 4096 Nov  6 14:43 .\n-rw-rw-rw-  1 nobody nogroup 5783941 Nov  6 14:43 sympy.whl\nShell cwd was reset to /testbed';
  const other = text.replaceAll('14:43','14:46');
  const canonical = tmpListing({text,tool});
  assert.equal(tmpListing({text:other,tool}), canonical);
  assert.notEqual(tmpListing({text:other.replace('5783941','5783942'),tool}), canonical);
  assert.notEqual(tmpListing({text:other.replace('sympy.whl','different.whl'),tool}), canonical);
  assert.equal(tmpListing({text,tool:{...tool,arguments:{command:'cat output'}}}),text);
  const warning = text + '\nls: denied';
  assert.equal(tmpListing({text:warning,tool}), warning);
});

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
  assert.equal(grepOrder({text:a,tool:truncated}), grepOrder({text:b,tool:truncated}));
  assert.notEqual(grepOrder({text:a.replace('foo','different'),tool:truncated}), grepOrder({text:b,tool:truncated}));
  const alternate = {...tool, arguments:{command:'grep -n "_latex" -r sympy/vector/ | head -2'}};
  assert.equal(grepOrder({text:a,tool:alternate}), grepOrder({text:b,tool:alternate}));
  const unrelated = {...tool, arguments:{command:'cat log'}};
  assert.equal(grepOrder({text:a,tool:unrelated}), a);
  const multiple = {...tool, arguments:{command:'grep -rn "BaseScalar" sympy/printing/*.py sympy/vector/*.py | head -50'}};
  const first = 'sympy/vector/z.py:2: foo\nsympy/printing/a.py:3: bar';
  const second = 'sympy/printing/a.py:3: bar\nsympy/vector/z.py:2: foo';
  assert.equal(grepOrder({text:first,tool:multiple}), grepOrder({text:second,tool:multiple}));
  const foreign = first.replace('sympy/vector/z.py','outside/z.py');
  assert.equal(grepOrder({text:foreign,tool:multiple}), foreign);
  const filtered = {...tool, arguments:{command:'grep -rn "x" sympy/vector/ sympy/printing/ 2>/dev/null | grep -v "test.py" | head -50'}};
  assert.equal(grepOrder({text:first,tool:filtered}), grepOrder({text:second,tool:filtered}));
  const included = {...tool, arguments:{command:'grep -rn "x" sympy/ --include=*.py | head -50'}};
  assert.equal(grepOrder({text:first,tool:included}), grepOrder({text:second,tool:included}));
  const wrongExtension = first.replace('z.py','z.txt');
  assert.equal(grepOrder({text:wrongExtension,tool:included}),wrongExtension);
  const compound = {...tool, arguments:{command:'git config --global --add safe.directory /testbed; cd /testbed && git log --oneline -2 && grep -rn "x" sympy/vector/ | head -50'}};
  const history = 'abcdef0123 first\n1234567890 second\n';
  assert.equal(grepOrder({text:history+a,tool:compound}),grepOrder({text:history+b,tool:compound}));
  assert.notEqual(grepOrder({text:history.replace('first','changed')+a,tool:compound}),grepOrder({text:history+b,tool:compound}));
});

test('long nonmatching grep commands do not exhaust the normalizer deadline', async () => {
  const source = readFileSync(new URL('../assets/examples/normalizers/grep-recursive-order.mjs', import.meta.url),'utf8');
  const supervisor = new NormalizerSupervisor([{id:'grep',source}], {callTimeoutMs:100});
  try {
    const text = 'unchanged';
    const tool = {name:'Bash',arguments:{command:'grep -rn "x" ' + 'a'.repeat(10000) + '!'}};
    assert.equal(await supervisor.normalize({text,tool}),text);
  } finally { await supervisor.close(); }
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
    assert.equal(normalize({ text, tool: {...tool,name:'Bash'} }), expected);
    assert.equal(normalize({ text, tool: {...tool,name:'shell_command'} }), expected);
    assert.equal(normalize({ text: expected, tool }), expected);
    assert.equal(normalize({ text, tool: null }), text);
    assert.equal(normalize({ text, tool: { name: "bash", arguments: { command: "cat output" } } }), text);
  }
});

test('all shell tool spellings retain command guards for pytest, unittest and diff', () => {
  const cases = [[pytest,'python -m pytest tests','1 passed in 1.25s\n'],
    [unittest,'python -m unittest tests','Ran 3 tests in 1.25s\n\nOK\n'],
    [diff,'diff -u before after','--- before\t2026-01-01 10:10:10 +0000\n+++ after\t2026-01-01 10:10:11 +0000\n']];
  for (const [normalize,command,text] of cases) {
    const expected = normalize({text,tool:{name:'bash',arguments:{command}}});
    for (const name of ['Bash','shell_command']) {
      assert.equal(normalize({text,tool:{name,arguments:{command}}}),expected);
      assert.equal(normalize({text,tool:{name,arguments:{command:'cat file'}}}),text);
    }
  }
});
