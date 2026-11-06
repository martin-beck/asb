# Pi native tool replay: external requests and workspace effects

The Pi SymPy qualification on 2026-11-05 recorded a successful GitHub API
request with `--llm-record`; two of four `--llm-replay` replicas later received
rate-limit errors from the real API. The mismatch is correct. LLM-only replay
records provider traffic, while native shell tools continue to execute. An
official passing grade from an incomplete conversation is not replay success.

Pi's existing sealed local-tool mode replaces its built-in tools, including
the shell, with an allowlisted argv service. It does not preserve the native
shell's interface or arbitrary behavior. Main also contains experimental Pi
native dispatch and Btrfs snapshot machinery with bounded sidecars; it needs
the complete coverage and qualification below before it can be presented as
a general native replay solution. Do not describe either mode, a proxy, or a
filesystem snapshot alone as transparent replay of this workload.

## Target and boundaries

Offer an explicit Pi native-tool recording and replay mode which retains Pi's
native tool schemas and behavior while recording each tool boundary, result,
and supported workspace effects. A complete replay must serve those results
and apply effects without launching the recorded commands or reaching the
original network. LLM-only flags retain their current meaning. Existing
cassettes without the required complete native sidecar remain usable for their
original mode and are not promoted into a native replay claim.

This does not claim deterministic execution of arbitrary agent-owned direct
processes, sockets, background services, all filesystem roots or kernel APIs.
Concurrent tool calls, descendant processes, open descriptors, non-workspace
paths, symlinks, special files and size limits require explicit coverage. If
an observed effect cannot be recorded and reconstructed faithfully, mark the
sidecar incomplete and refuse the corresponding replay; never fall back to
live execution. Do not change the provider tape, task tests or grades to hide
a real difference. Cassettes can contain private tool output and must inherit
the existing private artifact and retention contract.

For experimental snapshot replay, ASB disconnects the owned task container
from routed Docker networks before starting the agent. The private provider
relay continues over Docker exec and container loopback. This blocks direct
connections through those Docker networks, but does not by itself prove that
all tools were captured, that all filesystem effects are covered, or that a
configured loopback service cannot itself reach another network.

For a selected track with both a verified native-tool sidecar and workspace
snapshots, tool replay selects experimental snapshot mode automatically for
the supported SWE-bench workload family. Provider-only replay does not select
it. Opt in to recording when creating a configuration with
`asb setup --native-snapshot-record` or press Ctrl-N in the setup TUI, then
run or sweep that configuration with `--record`. The choice is
stored in each benchmark configuration and applies when tool recording is
selected; an ordinary live run does not create a snapshot cassette. The
experimental mode has been qualified for the Pi/SWE-bench route and requires a snapshot-capable Linux guest and available
`btrfs`, `mkfs.btrfs`, and `rsync` tools. Replay restores the
recorded extra-root layout when present; an explicit override must match it.
Automatic mode selection is not evidence of complete filesystem coverage.
For newly completed Pi snapshot replays, comparison evidence reports
`native_replay_consumed` only after the guest broker settles successfully. It
indicates that the captured native tool stream was consumed, not that every
agent process or effect was captured. Historical or missing observations remain
unknown; a task grade is not a substitute for this receipt.
New Pi native recordings bind requests to Pi tool-call IDs, so overlapping
calls with identical arguments can retain distinct outcomes. Older sidecars
lack these IDs: sequential replay remains compatible, while observed concurrent
identical calls are rejected as ambiguous. Older sidecars cannot prove every
historical overlap was distinguishable; they are not promoted to full coverage.
New run and comparison records also disclose the selected provider, tool and
MCP replay modes. Provider-only selection warns that native tools can still
execute and contact external services. Selection is not evidence that tools or
filesystem effects were captured completely; older records remain unknown.

## Implementation sequence

1. Define a Pi-owned, versioned native tool event contract with stable call
   identities, ordered start/end and result bytes, bounded resources and
   declared effects. Bind it to the cassette track, initial material and
   tool protocol; interrupted, ambiguous and unconsumed recordings are
   incomplete. Preserve the native Pi interface and keep Pi-specific parsing
   in its adapter. The common runner handles opaque sidecar custody only.
2. Capture and restore supported workspace mutations at each tool boundary,
   including failures and metadata where semantically relevant. Handle
   overlapping calls explicitly rather than imposing an invented order.
   Establish cleanup and crash-recovery receipts without using a positive
   workload cleanup as proof of host-process quiescence. Evaluate existing
   experimental snapshot branches against these requirements before reuse.
3. Implement replay in a loopback-only isolated guest with an independent
   cursor per replica. Validate each tool name and arguments, reproduce the
   recorded result and effects, and reject missing, extra, reordered or
   incompatible boundaries. The original shell and external services must
   not execute. Unsupported behavior produces an actionable incomplete result.
4. Expose the mode and its coverage clearly in CLI/TUI and comparison
   evidence. Distinguish provider-only, adapter-local and native modes; do
   not promise offline execution for a provider-only cassette. Show whether
   tool and filesystem coverage is complete and whether each replay consumed
   its entire track. Keep measurements of record and replay separate.
5. Qualify offline fixtures before a real run: use a private loopback HTTP
   server and a counter to prove a recorded shell request is not repeated;
   test matching and changed requests, failed commands, writes/deletes,
   concurrency, incomplete capture, cancellation and C1-to-C4 isolated
   replay. Then qualify one installed Pi client on the original SymPy task
   with a fresh recording, four replay conversations, official grades and
   both workload and process cleanup evidence. Preserve negative grades.

Transparent generic HTTPS interception is not assumed: certificate pinning,
credentials, non-HTTP traffic and indirect network access make a proxy alone
insufficient. Recording the complete native tool boundary and supported
effects avoids rerunning the reported `curl` command on replay; full coverage
must be evidenced, not inferred from a successful isolated test.
