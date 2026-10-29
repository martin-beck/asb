# Lightweight replay architecture

ASB replay is split by the interface an agent uses. The split keeps ordinary
agent runtime measurements in place while avoiding a second, system-wide
recorder, a filesystem image, or syscall-by-syscall capture.

## Layers

| Interface | Record path | Replay path | Current boundary |
| --- | --- | --- | --- |
| Provider API | The `llmtape` `ProxyAdapter` records the selected request/response exchange. | The same loopback ingress returns the next sealed response and validates request order and normalized content. | Selected provider endpoints only. |
| Declared stdio MCP server | The adapter substitutes ASB's transparent MCP relay, which forwards framed protocol bytes and emits a bounded sidecar. | The relay is served from the sealed sidecar; the declared server is not started. | Adapters that declare MCP relay support. |
| Adapter-owned local tools | An adapter routes its exposed file/search/command tools to the local-tool service, which records canonical calls, results, and confined workspace effects. | The service consumes the next sealed call, returns its result, and applies the recorded effects without running the original command. | Adapters that declare complete local-tool interception. |
| Native process lifecycle | The process gate emits a compact process trace used alongside ordinary process-scoped measurements. | The trace is validated and made available as a sealed sidecar. | Lifecycle evidence, not generic tool or filesystem emulation. |

The provider chain is ordered from agent to provider. `llmtape` is the default
record/replay hop; additional registered hops can be named in `ASB_PROXY_CHAIN`.
The agent receives the chain's loopback ingress, so its provider configuration
does not change when a hop is added or removed.

## Adapter-owned replay extensions

Record/replay has a deliberately narrow shared core.  The core may create an
attempt, select a mode, start a transparent protocol relay, own bounded private
files, seal their byte count and digest, stage the selected bytes for replay,
and report completion.  It must not know an agent name, a tool name, a
tool-call field, a client-specific identifier, a workspace-effect format, or a
client wire protocol.

Every agent-specific record/replay capability instead belongs to that agent's
adapter or to a private extension module immediately beside it.  The extension
owns all of the following:

- injecting the agent's hook, extension, MCP configuration, or wrapper;
- the sidecar kind and version, its payload codec, and its bounded validation;
- request identity, allowed normalization, ordering, cursor consumption, and
  mismatch diagnostics;
- conversion between the client protocol and a recorded result; and
- adapter-focused record, replay, rejected-mismatch, and no-live-effect tests.

The generic cassette and runner layers handle an extension sidecar as opaque
bytes plus a stable extension descriptor.  They can reject absent, oversized,
changed, duplicate, incomplete, or unconsumed sidecars, but never deserialize
their payload to decide how an agent tool, terminal, filesystem, or SDK call
behaves.  An adapter creates the replay consumer after the core has staged the
sealed bytes.  This preserves one common custody and lifecycle implementation
without turning it into a second generic agent protocol.

For example, an OpenHands terminal bridge owns its terminal-action and
terminal-observation schema; a Hermes extension owns its registered terminal
handler and session-ID normalization; and an OpenJiuwen extension owns its
grep/search protocol.  The shared code sees only each extension's descriptor
and private bytes.  A transparent MCP relay is a separate exception only at
the framing level: it may relay MCP messages without understanding a server's
tool names or payload semantics.

### Extension contract

An adapter extension has three lifecycle hooks:

1. **Prepare:** given the selected mode and cassette identity, install the
   client's normal hook or wrapper and declare one or more private artifact
   descriptors.  It cannot ask the runner to interpret client data.
2. **Collect:** after the process settles, validate and return its own bounded
   artifacts.  The core seals these unchanged with the provider track.
3. **Replay:** given only the staged, sealed descriptor and byte stream, build
   the client's replay hook.  It must fail closed on a changed request,
   exhausted stream, invalid schema, or remaining event; it must not start the
   original native tool or reach a live backend.

The extension's stable identifier includes its adapter and schema version, so
an incompatible codec is rejected before launch.  Extension diagnostics are
bounded and redact payloads, prompts, credentials, host paths, and raw tool
output.  Generic diagnostics identify only the extension descriptor and the
custody failure.

This also defines where shared code may evolve.  A reusable *opaque* file
sealer, descriptor registry, framing implementation, or timing cursor can be
shared.  A reusable semantic terminal/tool transcript, client ID normalizer,
or result applier cannot: it stays in an adapter extension, even if two
adapters currently look similar.  Similar extensions may later extract a
small explicitly-versioned adapter-extension library, but it remains outside
the generic cassette/runner path and is adopted explicitly by each adapter.

### Migration rule

Existing generic native-sidecar parsing is transitional.  New adapters must
not add fields or behavior to it.  Its semantic event decoding, terminal or
workspace-effect validation, and replay cursor will move to the owning adapter
extensions; the generic layer will retain only opaque sidecar custody,
aggregate bounds, and completion accounting.  During that migration, an
adapter must prove that its own extension—not the generic runner—rejects a
changed native request and prevents the original tool from starting during
replay.

## Commands and selection

`--llm-record` and `--llm-replay` select only provider traffic. `--tool-record`
and `--tool-replay` select the adapter-owned local-tool layer. `--mcp-record`
and `--mcp-replay` select declared MCP transport. The legacy `--record` and
`--replay` select the LLM and local-tool layers together. `--cassette` chooses
their shared cassette carrier, and `--respect-record-timings` replays recorded
provider response delays.

Replay rejects a missing cassette, a changed selected request, invalid sidecar,
or unconsumed local-tool event. It does not silently substitute a live provider
or an unrecorded declared MCP server. An explicit local-tool request is rejected
before launch when its adapter has not declared complete interception.

## Direct APIs and native subprocesses

No portable mechanism can transparently replay arbitrary direct file, process,
or kernel API calls made inside every third-party agent without interposition or
a filesystem/kernel virtualization layer. ASB deliberately does not claim that
its compact native trace provides this: it retains process-lifecycle evidence
and preserves runtime measurement, but does not emulate arbitrary native calls.

Consequently, a complete tool replay guarantee is adapter-specific. An adapter
must either route every benchmark-exposed tool through its local-tool transport
or decline that capability. New agent adapters should first use a documented
client extension/MCP ingress where one exists; a proposed native capture backend
must demonstrate bounded capture, per-process scope, exact replay semantics,
and no loss of the existing measurement target before it is registered.

## Resource contract

The design is intentionally light on disk and kernel work:

- the LLM proxy has one private session directory per execution and records
  selected exchanges rather than packet or syscall streams;
- MCP relay payloads, local-tool transcripts, and native trace indexes are
  bounded, sealed artifacts; replay sidecars are staged as files rather than
  copied through controller messages;
- the native trace index contains opaque content identities, not command lines,
  paths, prompt text, credentials, or output;
- runner capture continues to drain streams while retaining bounded prefixes;
  runtime collectors remain process-scoped and are independent of replay;
- a replay gets its own proxy cursor and workspace. Recording time, replay
  time, and collector data remain separately labelled in reports.

## Verification matrix

Each replay-layer change needs focused offline coverage before a live
qualification:

1. provider: strict request mismatch, full consumption, and optional timing;
2. MCP: record/serve framed traffic and prove replay starts no upstream server;
3. local tools: record/replay a tool-using adapter, changed/reordered/missing/
   unconsumed sidecar rejection, and no subprocess execution in replay;
4. native additions: process-tree identity and collector attachment, bounded
   artifact publication, and no system-wide tracing assertion;
5. end-to-end: one installed agent, one selected workload trial, and one
   provider record followed by an offline replay, retaining runtime and outcome
   artifacts separately.

Live-provider smoke tests are local qualifications, not CI requirements. They
must use one agent and one explicit workload trial before broader sweeps are
promoted.
