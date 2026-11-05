# Replay tool-result normalizers

LLM replay executes local tools again. Some tool output includes incidental
timings or generated values that can differ even when the useful output is
unchanged. A project can opt in to scripts that canonicalize selected tool
result text for cassette comparison. Scripts do not change what the agent's
tool returns or what ASB forwards to the live provider.

Declare profiles in `.asb/project.toml` and put modules under the project
root. For example, copy `crates/asb-cli/assets/examples/normalizers/sympy-test-banner.mjs`
to `normalizers/sympy.mjs` in the project and add:

```toml
[[replay.normalizers]]
id = "sympy-test-banner-v1"
module = "normalizers/sympy.mjs"
agents = ["pi", "codex"]
workload_families = ["swe-bench"]
tasks = ["sympy__sympy-*"]
```

At least one of `workloads` (actual workload adapter IDs) or
`workload_families` is required. `agents` and `tasks` are optional. Values in
each list are alternatives; populated lists are combined with AND. Missing
family metadata never matches a family selector. Task patterns are
case-sensitive, match the whole task ID, and support only `*` and `?`.
Profiles are applied in declaration order. Paths are relative to the project
root and cannot escape it. ASB freezes the selected source bytes for the
campaign; changing the profile or source prevents continuation against the
old policy.

Each module exports one synchronous function:

```js
export function normalizeToolResult({ text, workload, tool }) {
  // workload: { id, family, task_id }
  // tool: { name, arguments } or null when no call can be associated
  return text;
}
```

Return the input unchanged unless the tool, command and output are the
intended case. The tool arguments are decoded JSON when available; ASB never
guesses an ambiguous call association. Scripts see individual text blocks
from supported Responses, Chat Completions and Anthropic tool results, not
prompts, assistant messages, tool arguments or other request fields. They
must return a string and should be deterministic and idempotent. Scripts run
in a bounded worker; this is not a security boundary for untrusted code.
If a selected module cannot start, ASB identifies the profile and reports a
bounded error before launching the agent; module exception text is not printed.

The repository also provides opt-in examples for Python unittest and pytest
summary timings, unified-diff header times, OpenCode search ordering, shell
`ls` diagnostics, Python representation addresses, and pip transfer progress.
Select those examples only for the relevant agents, workloads and tasks; each
requires an associated matching tool or shell command. Canonicalizing output
does not make tool execution deterministic or establish that a benchmark passed.

Record a new cassette with the selected profiles before replaying. A profile
cannot be silently added to an older recording, and an archive never supplies
executable scripts: replay uses the explicitly configured local modules and
requires their frozen policy to match. Runs without selected profiles retain
their historical matching contract. Normal live runs do not run these scripts.

Private tape evidence contains the effective policy digest and per-profile
observation and change counts. These count visits to result blocks, including
the same history revisited in later requests; they are not counts of distinct
tool executions. A separate private normalization evidence file is retained
beside the tape session on shutdown, even when measurement collectors are
disabled. Replay mismatch evidence also includes the active policy and counts.
Up to 64 changed result visits include before/after SHA-256 digests and the
IDs of profiles that changed the text; the evidence does not include the text.
Campaign comparison JSON and HTML show the planned policy and, when a
completed attempt retained valid observations, its observed counts separately.
