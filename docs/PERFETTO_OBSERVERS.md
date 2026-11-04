# Perfetto proxy and tool observations

Perfetto already owns an isolated tracefs instance and a process-scoped
capture.  Request and tool intervals must not give child processes a tracefs
pathname, host tracing access, or a way to affect a benchmark result.  They use
one collector-owned observation relay instead.

## Capability and lifetime

When the `perfetto` collector is selected, its session creates a private,
bounded, inherited descriptor for an observation relay.  Only the built-in
llmtape proxy and ASB-owned local-tool/MCP relay receive a duplicate of that
descriptor.  Overrides and arbitrary agent children do not receive it.

The relay is started before proxy preparation but does not write trace markers
until the collector has attached the benchmark process and opened its pinned,
project-local `trace_marker`.  It then translates content-free observation
messages into paired asynchronous `ATRACE` slices.  The marker is retained by
the collector, not reopened by a child.  Loss of the relay, a full queue, or a
marker write failure records incomplete observation evidence but never changes
an HTTP/MCP/tool result, cassette, official evaluator result, or independently
valid scheduler measurement.

The parent stops admission at `RunFinishing`/`RunFailed`, drains a bounded
number of already accepted messages, records unfinished intervals, writes its
owned evidence document, and then releases the Perfetto service.  This order
prevents a proxy or relay from writing after tracing teardown.

## Wire format and privacy

The inherited endpoint transports a bounded line-oriented event vocabulary:

```
begin <kind> <cookie>
end <kind> <cookie>
instant <kind> <cookie> <reason>
```

`kind` is an ASB-owned fixed token.  `cookie` is a locally allocated bounded
integer.  The wire format deliberately has no URL, model, request body,
headers, tool name, path, argument, response, or JSON-RPC identifier.  The
relay rejects malformed, oversized, or out-of-order input and accounts for it
in the observation document.

The emitted trace names are fixed prefixes: `asb.llm.request` and `asb.tool`.
They include only the session trace identity, a fixed operation category, and
the opaque cookie.  This makes concurrent work searchable in the official
Perfetto UI while keeping provider and workload content out of trace markers.

## Producer-specific contracts

The embedded llmtape proxy opens a request interval immediately after accepting
an HTTP request and closes it after the loopback response has finished.  Both
record and replay use the current request timing; replay does not reproduce
historical timestamps.  HTTP error and cancellation paths emit a fixed instant
event and still settle the interval where a response was observed.

The local-tool service and MCP relay begin before dispatch/forwarding and end
only after the result, filesystem effects, or relay response has settled.
MCP correlates requests with JSON-RPC IDs only in private relay memory; IDs are
never put in markers or evidence.  Notifications, initialization, and
`tools/list` are not invocation intervals.  A missing terminal event remains
unfinished rather than being guessed successful.

Each producer writes a separate bounded JSON document (`perfetto-requests.json`
or `perfetto-tools.json`) in the measurement directory.  It contains schema and
trace identity, aggregate counts, completeness, bounded error categories, and
whether a limit was reached.  The Perfetto collector validates that the document
belongs to the current session and that no declared unfinished/malformed state
is silently treated as complete.
