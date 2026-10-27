# Layered record and replay delivery plan

ASB already replays provider traffic and its adapter-local tool transport.  A
full replay of an agent's interactions with its environment is a separate
capability.  This plan introduces it without changing normal execution, making
recording a security boundary, or adding an `LD_PRELOAD`/syscall tracing path.

## Scope and invariants

The finished design has three independent, composable layers:

1. **LLM proxy chain.** The existing provider-neutral loopback chain records
   and replays provider protocol traffic.  It continues to own
   `--llm-record`/`--llm-replay`.
2. **MCP relay.** A generic stdio relay records and replays MCP protocol
   messages between an agent and an ordinary MCP server.  The agent receives a
   normal MCP server configuration; it does not need a replay-specific tool
   API.  ASB's current local-tool MCP server remains a separate semantic tool
   transport until the relay has equivalent coverage.
3. **Native capsule.** An explicit opt-in recording/capsule mode reconstructs
   recorded executable, filesystem-input, and declared-workspace-effect state.
   It is transparent compatibility machinery, not a sandbox or authorization
   mechanism.  The agent continues to use its normal paths, direct APIs, and
   subprocesses.

No layer may silently fall back to a live provider, host MCP server, or host
tool during replay.  Existing `--replay` stays LLM plus adapter-local tool
replay.  It does **not** assert executable identity or claim a native capsule
until the native mode below is complete.

All durable indexes are bounded and content-addressed.  They never serialize
host PIDs, paths, command lines, prompts, credentials, raw tool output, or a
second complete copy of agent stdout/stderr.  Measurements remain attached to
the actual agent process tree.  Full syscall traces, kernel-wide tracing,
sampling loops, FUSE interception, and `LD_PRELOAD` are explicitly out of
scope.

## Delivery steps

### 1. Shared bounded MCP framing

Extract the currently duplicated stdio framing rules into one private module:
JSON-lines and `Content-Length` messages, an exact maximum message size, and
bounded parse/write errors.  Migrate the private local-tool server to it with
byte-for-byte framing tests.

**Acceptance:** malformed or oversized input is rejected; both framing styles
round-trip; existing local-tool record/replay tests remain unchanged.

### 2. MCP transcript contract

Add a versioned JSONL protocol transcript that records direction, a monotonic
sequence number, a bounded relative timestamp, message digest, byte count, and
a private payload reference.  It supports requests, responses, notifications,
errors, and concurrent/out-of-order response IDs.  Payload storage is
deduplicated, bounded, and owned by the recording; the index alone never
contains protocol payloads.

**Acceptance:** canonical encode/decode, unknown-field rejection, order and
response-ID mismatch diagnostics, notification coverage, and bounded-input
tests pass offline.

### 3. Transparent MCP relay in record mode

Implement an ASB-owned stdio relay process.  It forwards both directions
without interpreting tool names or requiring a particular MCP server.  It
preserves the negotiated framing and records the protocol contract from step
2.  It owns child startup, pipe closure, bounded cancellation, and transcript
publication only after clean relay completion.

**Acceptance:** a loopback MCP fixture with client notifications and multiple
in-flight requests observes identical traffic through the relay; interrupted
or malformed sessions publish no complete transcript; no live provider or
installed agent is required in CI.

### 4. Transparent MCP relay in replay mode

Replay starts no upstream MCP server.  It checks each client message against
the recorded direction, normalized payload, sequence rules, and ID mapping,
then emits only the matching recorded server messages.  Optional recorded
delays use the existing bounded timing policy.  EOF requires full transcript
consumption.

**Acceptance:** offline record-then-replay succeeds; mismatched request,
unexpected notification, unconsumed message, missing transcript, and attempted
upstream launch all fail closed.  The test fixture proves no network or host
tool call occurs in replay.

### 5. Adapter integration and explicit user interface

Add an explicit MCP-layer switch only for adapters that can route ordinary MCP
configuration through the relay without disabling unrelated agent behavior.
Configuration continues to describe the real server; the runner substitutes
the relay ingress transparently.  The existing local-tool transport is not
removed until its semantic workspace-effect coverage is reproduced and its
regressions pass.

The benchmark contract represents each declared stdio server as a bounded
`[[mcp_servers]]` record with a stable lowercase `id`, `program`, and an
argument array.  ASB validates the declaration without launching it; adapter
configuration owns materialization.  This preserves no-shell execution, makes
the same server declaration usable for record and replay, and prevents an
unknown or duplicate server from silently changing a benchmark.

```toml
[[mcp_servers]]
id = "filesystem"
program = "mcp-filesystem"
args = ["."]
```

**Acceptance:** each opted-in adapter has record, replay, missing-layer, and
cleanup tests.  An adapter with no completed MCP route rejects the requested
MCP layer rather than degrading to a live server.

### 6. Capsule manifest and snapshot backend

Define a distinct, opt-in native-capsule manifest.  It binds a recording to
its bounded root executable identity (when available), private dependency/input
objects, a base workspace snapshot, and declared final workspace effects.  The
backend uses whole-tree snapshots and content-addressed objects at controlled
boundaries; it does not intercept every filesystem API or syscall.

**Acceptance:** manifest schema validation, object digest/size checks,
symlink/regular-file custody handling, refusal of missing or changing input,
and preservation of provider-only legacy recordings.  A new native-capsule
replay flag is required; generic `--replay` remains valid when an agent script
has deliberately changed after a normal LLM/tool recording.

### 7. Explicit native-capsule replay

Stage a fresh copy-on-write workspace from the selected manifest and private
objects, then apply recorded final effects only at the declared boundary.  Run
the agent normally inside that reconstructed workspace.  Direct APIs and
subprocesses remain ordinary operations; they are repeatable only to the extent
the recorded capsule supplies their inputs and effects.  The implementation
must state unsupported dynamic host interactions instead of pretending to
replay them.

**Acceptance:** an offline fixture records then replays a script which reads,
writes, and spawns a known project-local executable; fresh replay has the
recorded input/effect state and no dependency on the original workspace.
Changed executable identity is rejected **only** in this explicit native mode.
Collectors still attribute runtime to the real agent tree.

### 8. End-to-end qualification and operational limits

Qualify one supported agent, one local MCP fixture, and one small SWE-bench
trial with cli2key in live record followed by offline LLM/MCP/native replay.
The qualification retains only bounded cassette/capsule artifacts and uses the
normal process-scoped collectors.  It reports which layers were active and
which environment interactions were outside the capsule.

**Acceptance:** successful online recording, successful offline replay with
outbound provider/MCP denial, bounded disk use, preserved measurement output,
and clear failures for absent prerequisite layers.  This is the first point at
which ASB may describe the selected mode as full tool/environment replay.

## Non-goals

This work does not impose an agent sandbox, forbid direct APIs, virtualize a
whole host filesystem, replay arbitrary external network traffic, or use the
replay mechanism for security enforcement.  A future optional namespace or
copy-on-write backend may improve fidelity, but its presence is never a reason
to alter normal agent command construction or collectors.
