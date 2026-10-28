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
