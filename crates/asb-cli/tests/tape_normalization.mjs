import { test } from "node:test";
import assert from "node:assert/strict";
import { normalizeWorkspacePaths } from "../assets/scripts/tape-normalization.mjs";

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
