import test from "node:test";
import assert from "node:assert/strict";
import { normalizeRequest as codex } from "../assets/scripts/agent-normalizers/codex.mjs";
import { normalizeRequest as hermes } from "../assets/scripts/agent-normalizers/hermes.mjs";
import { normalizeRequest as openhands } from "../assets/scripts/agent-normalizers/openhands.mjs";
import { normalizeRequest as workbuddy } from "../assets/scripts/agent-normalizers/workbuddy.mjs";

test("WorkBuddy normalizes only generated persisted tool-result paths", () => {
  const path = (session, suffix, tool = "run_command") => `/opt/asb-private/agent-state/workbuddy/config/projects/testbed/${session}/tool-results/mcp-asb-local-tools-${tool}-${suffix}.txt`;
  const first = path("123e4567-e89b-12d3-a456-426614174000", "1794046319666-380df9");
  const second = path("123e4567-e89b-12d3-a456-426614174001", "1794046379340-905948");
  const normalize = (p, role = "tool") => workbuddy({ profileActive: true, body: { messages: [{ role, content: `Output saved to ${p}` }] } }).messages[0].content;
  assert.equal(normalize(first), normalize(second));
  assert.notEqual(normalize(first, "user"), normalize(second, "user"));
  assert.notEqual(normalize(first), normalize(path("123e4567-e89b-12d3-a456-426614174001", "1794046379340-905948", "read_file")));
  assert.notEqual(normalize(first), normalize(second.replace("/testbed/", "/elsewhere/")));
});

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

test("WorkBuddy normalizes only the generated environment OS version", () => {
  const generated = (version) => ({
    profileActive: true,
    body: {
      messages: [{
        role: "system",
        content: `<env>\nPlatform: linux\nOS Version: ${version}\nDefault shell: sh\n</env>`,
      }],
    },
  });
  assert.deepEqual(
    workbuddy(generated("#3 SMP PREEMPT Mon Jul 13 11:43:49 UTC 2026")),
    workbuddy(generated("#1 SMP Mon Nov 9 19:32:10 CST 2026")),
  );
  const literal = { body: { messages: [{ role: "system", content: "OS Version: retain this literal" }] } };
  assert.equal(workbuddy(literal).messages[0].content, literal.body.messages[0].content);
});

test("profile mode leaves Codex workload summaries for selected scripts", () => {
  const output = "Ran 2 tests in 1.20s\n\nOK\n";
  const body = { input: [{ type: "function_call_output", output }] };
  assert.equal(codex({ body, profileActive: true }).input[0].output, output);
  assert.notEqual(codex({ body }).input[0].output, output);
});
