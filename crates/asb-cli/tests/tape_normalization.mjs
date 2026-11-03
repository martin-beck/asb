import { test } from "node:test";
import assert from "node:assert/strict";
import { normalizeWorkspacePaths } from "../assets/scripts/tape-normalization.mjs";
import * as normalization from "../assets/scripts/tape-normalization.mjs";

test("streamed reasoning keeps a split guest path and remaps a split host path", () => {
  assert.equal(typeof normalization.normalizeOpenAiSseWorkspacePaths, "function");
  const chunks = [
    "/asb-snapshot-storage/snapshot/files",
    "ystem/workspace/a and /srv/record/workspace-con",
    "currency-1/b",
  ];
  const body = chunks.map((reasoning) => `data: ${JSON.stringify({choices:[{index:0,delta:{reasoning}}]})}\n\n`).join("") + "data: [DONE]\n\n";
  const output = normalization.normalizeOpenAiSseWorkspacePaths(body, "/srv/record/workspace-concurrency-1", "<asb-workspace>");
  const values = output.split("\n").filter((line) => line.startsWith("data: {")).map((line) => JSON.parse(line.slice(6)).choices[0].delta.reasoning);
  assert.equal(values.join(""), "/asb-snapshot-storage/snapshot/filesystem/workspace/a and <asb-workspace>/b");
  assert.equal(values.length, chunks.length);
  assert.ok(output.endsWith("data: [DONE]\n\n"));
  const canonical = normalizeWorkspacePaths(output);
  const replay = normalization.normalizeOpenAiSseWorkspacePaths(canonical, "<asb-workspace>", "/srv/replay/workspace-concurrency-4");
  const replayValues = replay.split("\n").filter((line) => line.startsWith("data: {")).map((line) => JSON.parse(line.slice(6)).choices[0].delta.reasoning);
  assert.equal(replayValues.join(""), "/asb-snapshot-storage/snapshot/filesystem/workspace/a and /srv/replay/workspace-concurrency-4/b");
});

test("streamed tool arguments and reasoning details are independently reconstructed", () => {
  const events = [
    {choices:[{index:0,delta:{tool_calls:[{index:0,function:{arguments:'{"path":"/srv/record/work'}}],reasoning_details:[{text:"/asb-snapshot-storage/snapshot/files"}]}}]},
    {choices:[{index:0,delta:{tool_calls:[{index:0,function:{arguments:'space-concurrency-1/😀"}'}}],reasoning_details:[{text:"ystem/workspace/a"}]}}]},
  ];
  const body = events.map((value) => `data: ${JSON.stringify(value)}\n\n`).join("");
  const output = normalization.normalizeOpenAiSseWorkspacePaths(body, "/srv/record/workspace-concurrency-1", "<asb-workspace>");
  const parsed = output.split("\n").filter((line) => line.startsWith("data: {")).map((line) => JSON.parse(line.slice(6)));
  const deltas = parsed.map((event) => event.choices[0].delta);
  assert.equal(deltas.map((delta) => delta.tool_calls[0].function.arguments).join(""), '{"path":"<asb-workspace>/😀"}');
  assert.equal(deltas.map((delta) => delta.reasoning_details[0].text).join(""), "/asb-snapshot-storage/snapshot/filesystem/workspace/a");
});

test("every two-chunk split preserves guest paths through record and replay", () => {
  const guest="/asb-snapshot-storage/snapshot/filesystem/workspace/a";
  const record="/srv/record/workspace-concurrency-1";
  const replay="/srv/replay/workspace-concurrency-2";
  const input=`${guest} ${record}/😀`;
  for (let split=1; split<input.length; split++) {
    if (/[\uD800-\uDBFF]/.test(input[split-1]) && /[\uDC00-\uDFFF]/.test(input[split])) continue;
    const chunks=[input.slice(0,split),input.slice(split)];
    const body=chunks.map((content)=>`data: ${JSON.stringify({choices:[{index:0,delta:{content}}]})}\n\n`).join("");
    const canonical=normalizeWorkspacePaths(normalization.normalizeOpenAiSseWorkspacePaths(body,record));
    const output=normalization.normalizeOpenAiSseWorkspacePaths(canonical,"<asb-workspace>",replay);
    const text=output.split("\n").filter((line)=>line.startsWith("data: {")).map((line)=>JSON.parse(line.slice(6)).choices[0].delta.content).join("");
    assert.equal(text,`${guest} ${replay}/😀`,`split ${split}`);
  }
});

test("preserves the guest snapshot workspace while normalizing host workspaces", () => {
  const guest = "/asb-snapshot-storage/snapshot/filesystem/workspace/lib/widgets.py";
  const host = "/srv/data/projects/test/results/batch-1/workspace-concurrency-1/lib/widgets.py";
  assert.equal(normalizeWorkspacePaths(`diff ${guest} ${host}`), `diff ${guest} <asb-workspace>/lib/widgets.py`);
  assert.equal(normalizeWorkspacePaths(`diff ${guest} ${host}`, "/replay"), `diff ${guest} /replay/lib/widgets.py`);
});

function legacy(body) {
  return body
    .replace(/\/?(?:[A-Za-z0-9_.-]+\/)+workspace-concurrency-\d+(?:\.asb-agent-runtime)?/g, "<asb-workspace>")
    .replace(/\/?(?:[A-Za-z0-9_.-]+\/)+workspace(?=\/|["\\])/g, "<asb-workspace>");
}

test("bounded exhaustive and fixed-seed corpus preserves legacy workspace normalization", () => {
  const atoms = ["", "a", "workspace", "workspaceX", "workspace-concurrency-1", "workspace-concurrency-2.asb-agent-runtime", "x.workspace", "é", "éa", "1", "_", "-"];
  const check = (input) => assert.equal(normalizeWorkspacePaths(input), legacy(input), JSON.stringify(input));
  for (const a of atoms) for (const b of atoms) for (const c of atoms) {
    for (const prefix of ["", "/", "literal:", "é", "\\", '"']) {
      for (const suffix of ["", "/", '"', "\\", "tail", "?", "é"]) {
        check(`${prefix}${a}/${b}/${c}${suffix}`);
      }
    }
  }
  let seed = 0x12345678;
  const next = (width) => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed % width; };
  const separators = ["/", "//", ".", " ", '"', "\\", "é"];
  for (let row = 0; row < 5000; row += 1) {
    let input = "";
    const count = next(10) + 1;
    for (let index = 0; index < count; index += 1) input += atoms[next(atoms.length)] + separators[next(separators.length)];
    check(input);
  }
});

test("long non-path bodies and many-component paths do not retry at each byte", { timeout: 5000 }, () => {
  const plain = "x".repeat(12 * 1024 * 1024);
  assert.equal(normalizeWorkspacePaths(plain), plain);
  const path = "a/".repeat(20_000) + "not-a-workspace";
  assert.equal(normalizeWorkspacePaths(path), path);
  assert.equal(normalizeWorkspacePaths(path + '/workspace"'), '<asb-workspace>"');
});
