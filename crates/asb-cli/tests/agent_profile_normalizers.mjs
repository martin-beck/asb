import test from "node:test";
import assert from "node:assert/strict";
import { normalizeRequest as codex } from "../assets/scripts/agent-normalizers/codex.mjs";
import { normalizeRequest as hermes } from "../assets/scripts/agent-normalizers/hermes.mjs";
import { normalizeRequest as openhands } from "../assets/scripts/agent-normalizers/openhands.mjs";
import { normalizeRequest as workbuddy } from "../assets/scripts/agent-normalizers/workbuddy.mjs";

test("profile mode keeps historical recursive agent rewrites away from literal text", () => {
  for (const [normalize, text] of [
    [hermes, "/opt/asb-private/agent-state/tmp/hermes-verify-abc.py"],
    [openhands, "/openhands/conversations/abcdef12/observations/terminal_output_abcdef12.txt"],
    [workbuddy, "srv-data-projects-foo-sweep-record-xyz"],
  ]) {
    const body = { messages: [{ role: "user", content: text }] };
    assert.equal(normalize({ body, profileActive: true }).messages[0].content, text);
    assert.notEqual(normalize({ body }).messages[0].content, text);
  }
});

test("profile mode leaves Codex workload summaries for selected scripts", () => {
  const output = "Ran 2 tests in 1.20s\n\nOK\n";
  const body = { input: [{ type: "function_call_output", output }] };
  assert.equal(codex({ body, profileActive: true }).input[0].output, output);
  assert.notEqual(codex({ body }).input[0].output, output);
});
