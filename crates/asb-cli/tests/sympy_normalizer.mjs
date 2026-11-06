import test from "node:test";
import assert from "node:assert/strict";
import { normalizeToolResult } from "../assets/examples/normalizers/sympy-test-banner.mjs";

test("only a complete SymPy banner from a matching shell command loses its seed", () => {
  const text = "==== test process starts ====\nrandom seed: 123\n==== tests finished: 1 passed ====\n";
  const tool = { name: "bash", arguments: { command: "python -c 'sympy.test(...)'" } };
  const normalized = normalizeToolResult({ text, tool });
  assert.equal(normalized, text.replace("123", "<asb-seed>"));
  assert.equal(normalizeToolResult({ text: normalized, tool }), normalized);
  assert.equal(normalizeToolResult({ text, tool: { ...tool, name: "shell_command" } }), normalized);
  assert.equal(normalizeToolResult({ text, tool: null }), text);
  assert.equal(normalizeToolResult({ text, tool: { ...tool, name: "read" } }), text);
  assert.equal(normalizeToolResult({ text, tool: { ...tool, arguments: { command: "echo seed" } } }), text);
  assert.equal(normalizeToolResult({ text: text.replace("tests finished", "truncated"), tool }), text.replace("tests finished", "truncated"));
  assert.equal(normalizeToolResult({ text: text + text, tool }), text + text);
  const changed = text.replace("1 passed", "1 failed");
  assert.notEqual(normalizeToolResult({ text: changed, tool }), normalized);
});

test('normalizes only the hash seed in an intact SymPy metadata banner', () => {
  const tool = {name:'bash', arguments:{command:'python bin/test sympy/vector/tests/test_coordsysrect.py'}};
  const prefix = 'architecture: 64-bit\ncache: yes\nground types: python\nnumpy: None\nrandom seed: 123\nhash randomization: on (PYTHONHASHSEED=';
  const suffix = ')\nwarning: something\n tests finished: 1 passed, in 1.00 seconds\n';
  const record = prefix + '3849647123' + suffix;
  const replay = prefix.replace('123', '456') + '950920009' + suffix;
  const canonical = normalizeToolResult({text:record,tool});
  assert.equal(normalizeToolResult({text:replay,tool}), canonical);
  assert.equal(normalizeToolResult({text:canonical,tool}), canonical);
  assert.notEqual(normalizeToolResult({text:replay.replace('warning: something', 'warning: different'),tool}), canonical);
  assert.notEqual(normalizeToolResult({text:replay.replace('on (PYTHONHASHSEED=950920009)', 'off'),tool}), canonical);
  const ambiguous = replay + 'hash randomization: on (PYTHONHASHSEED=4)\n';
  assert.match(normalizeToolResult({text:ambiguous,tool}), /PYTHONHASHSEED=950920009/);
  const unrelated = {...tool, arguments:{command:'cat log'}};
  assert.equal(normalizeToolResult({text:replay,tool:unrelated}), replay);
});

test('normalizes duration while retaining exception and failure counts', () => {
  const tool = {name:'bash', arguments:{command:'python bin/test sympy/vector/tests/test_printing.py'}};
  const record = 'AssertionError\n===== tests finished: 122 passed, 1 failed, 2 exceptions, in 2.29 seconds ======\nDO *NOT* COMMIT!\n';
  const replay = record.replace('2.29 seconds', '2.42 seconds');
  const canonical = normalizeToolResult({text:record,tool});
  assert.equal(normalizeToolResult({text:replay,tool}), canonical);
  assert.notEqual(normalizeToolResult({text:replay.replace('2 exceptions', '1 exceptions'),tool}), canonical);
  assert.notEqual(normalizeToolResult({text:replay.replace('AssertionError', 'ValueError'),tool}), canonical);
});

test('short explicit tail normalizes an intact SymPy metadata suffix', () => {
  const tool = {name:'bash', arguments:{command:'python bin/test sympy/tests/test_x.py 2>&1 | tail -8'}};
  const text = 'cache: yes\nground types: python\nnumpy: None\nrandom seed: 64522818\nhash randomization: off\n\n\n tests finished: 0 passed, in 1.00 seconds\n';
  const other = text.replace('64522818','34443566');
  assert.equal(normalizeToolResult({text,tool}), normalizeToolResult({text:other,tool}));
  assert.notEqual(normalizeToolResult({text:other.replace('0 passed','1 passed'),tool}), normalizeToolResult({text,tool}));
  assert.match(normalizeToolResult({text,tool:{...tool,arguments:{command:'python bin/test sympy/tests/test_x.py'}}}), /random seed: 64522818/);
});

test('tailed SymPy metadata and elapsed summaries preserve substantive differences', () => {
  const tool = {name:'bash', arguments:{command:'cd /testbed && python bin/test sympy/example.py 2>&1 | tail -40'}};
  const metadata = 'architecture:       64-bit\ncache:              yes\nground types:       python \nnumpy:              None\nrandom seed:        12345\nhash randomization: off\n';
  const header = 'executable:         /opt/miniconda3/envs/testbed/bin/python  (3.9.20-final-0) [CPython]\n';
  const body = 'failure: expected value\n';
  const short = '= tests finished: 997 passed, 26 skipped, 11 expected to fail, in 9.99 seconds ===\n';
  const long = ' tests finished: 997 passed, 26 skipped, 11 expected to fail, in 11.10 seconds ===\n';
  for (const prefix of ['', header]) {
    const recorded = prefix + metadata + body + short;
    const replayed = prefix + metadata.replace('12345', '98765') + body + long;
    const canonical = normalizeToolResult({text: recorded, tool});
    assert.equal(normalizeToolResult({text: replayed, tool}), canonical);
    assert.equal(normalizeToolResult({text: canonical, tool}), canonical);
    assert.notEqual(normalizeToolResult({text: replayed.replace('997 passed', '996 passed'), tool}), canonical);
    assert.notEqual(normalizeToolResult({text: replayed.replace('expected value', 'different failure'), tool}), canonical);
    assert.notEqual(normalizeToolResult({text: replayed.replace(' seconds ===', ' seconds ====') , tool}), canonical);
    assert.notEqual(normalizeToolResult({text: replayed.replace(' tests finished:', '  tests finished:'), tool}), canonical);
    if (prefix) assert.notEqual(normalizeToolResult({text: replayed.replace('testbed/bin/python', 'other/bin/python'), tool}), canonical);
    const clipped = replayed.replace('cache:              yes\n', '');
    assert.match(normalizeToolResult({text: clipped, tool}), /random seed:        98765/);
    const duplicate = '==== test process starts ====\n' + replayed.replace('random seed:        98765', 'random seed: malformed\nrandom seed:        98765');
    assert.match(normalizeToolResult({text: duplicate, tool}), /random seed:        98765/);
  }
  const outputOnly = body + short;
  assert.equal(normalizeToolResult({text: outputOnly, tool}), normalizeToolResult({text: body + long, tool}));
  assert.equal(normalizeToolResult({text: outputOnly, tool:{name:'bash',arguments:{command:'cat result'}}}), outputOnly);
  const noRightPadding = '= tests finished: 1 passed, in 9.99 seconds\r\n';
  const noPadding = ' tests finished: 1 passed, in 11.10 seconds\r\n';
  const result = normalizeToolResult({text:noRightPadding,tool});
  assert.equal(normalizeToolResult({text:noPadding,tool}), result);
  assert.equal(normalizeToolResult({text:result,tool}), result);
});
