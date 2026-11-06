# Shared workspace snapshots

Snapshot storage and execution are separate axes. The execution adapter owns
its task environment and provides a private transport and workspace roots; a
snapshot backend owns preparation, boundary capture, archive restoration and
settlement within that environment. The agent adapter owns interception and
native tool event semantics. No snapshot backend can establish that a tool was
not re-executed merely by restoring files.

The first implementation wraps the existing Btrfs guest supervisors in a
backend interface. It retains the existing container transport and custody
choices. An execution VM must provide a compatible transport and its own
lifecycle; selecting a Btrfs filesystem image must not select a VM backend or
change an already selected execution environment. Other snapshot backends must
implement the same session lifecycle rather than modify agent protocols.

Users can select snapshot recording for untested configurations. The result
must disclose and fail on concrete missing archive, missing tool evidence,
unsettled capture, or a mismatch between agent events and workspace boundaries.
The archive, its backend format and declared roots must be identified and bound
to the selected recording. Replay requires a separate cursor and workspace for
each replica, validates the selected recording, restores effects at matching
boundaries, and must not run recorded tool commands or contact a live provider.
Failures retain their evidence. Unknown external roots and overlapping mutations
must not be presented as verified complete replay.

Remaining integration work:

1. Define workload-owned root and transport interfaces with actual startup and
   settlement evidence, including a native execution path and explicit separation
   from VM lifecycle. Remove workload-name dispatch only when the shared path
   records and replays effects for the selected workload.
2. Bind OpenCode, Claude Code and OpenJiuwen tool sidecars to the settled archive
   and validate marker ordering through each adapter. Preserve original tool
   behavior during recording and reject mismatched or incomplete replay.
3. Exercise each installed agent with a local deterministic provider, one real
   workload, workspace mutations and an external command sentinel. Verify an
   offline replay reproduces the effects without executing the sentinel; repeat
   with concurrent replicas and distinct workspaces. Preserve logs and report
   exactly which agents and workload combinations were tested.
4. Test alternate execution transports and failure/cleanup paths without
   weakening existing provider, tool, measurement or resource ownership gates.
