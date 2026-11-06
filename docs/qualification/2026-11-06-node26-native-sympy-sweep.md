# Native ARM SymPy sweep

On 2026-11-06, a native aarch64 node26 run used OpenRouter model
`deepseek/deepseek-v4.1-flash` with one agent replica per configuration. The
matrix covered 13 agents and original SWE-bench instances
`sympy__sympy-13091` and `sympy__sympy-14774`. The latter used ASB's derived
native ARM source image because the published ARM image was unavailable; it
is not a byte-identical upstream x86 environment. No VM or CPU emulator was
used. Each agent execution had a 1,800-second budget.

The initial 26-case sweep produced 21 collector failures and five agent
failures. The official evaluator reported resolution for 22 cases and no
resolution for four; a resolved task does not make an incomplete run a
successful benchmark. All five selected bpftrace profiles failed their
graceful-flush checks. Two OpenCode and two Claude Code executions could not
start because npm 12 had blocked their installation scripts; an OpenJiuwen
run reached its iteration limit despite a resolved evaluation.

PR #1251 repaired collector interrupt forwarding without accepting incomplete
captures. PR #1253 repaired existing npm installations using a staged install
that retains the previous tool tree. The affected project-local agent binaries
were reinstalled and their version commands succeeded. PR #1255 corrected
Hermes input accounting to include cached input.

A second 26-case native sweep ran with the collector and npm fixes. Its input
stability seal was stable. Twenty-two run records completed with resolved
tasks, two clean agent executions were unresolved, and two resolved tasks
were marked failed because of the OpenJiuwen iteration limit and Hermes perf
shutdown respectively. Some cases and follow-ups overlapped on the host, so
their wall times are affected by shared-host contention. Both Codex results
have completed successfully and the evaluator reported resolution. For
`sympy__sympy-14774`, each of the five bpftrace profiles has a completed capture.
Its perf, Perfetto, flamegraph, mpstat and iostat records also report
completion; agent memory and worker records report observations. CPU
accounting used process sampling because the host's cgroup CPU controller
was unavailable, so it can miss short-lived processes.

The table records original second-sweep outcomes. “Resolved” means the run
record completed and the official evaluator resolved the task; individual
collector limitations are stated separately below.

| Agent | 13091 | 14774 |
| --- | --- | --- |
| Codex | Resolved | Resolved |
| Pi | Unresolved | Resolved |
| OpenCode | Unresolved | Resolved |
| WorkBuddy | Resolved | Resolved |
| Qwen Code | Resolved | Resolved |
| Mini-SWE | Resolved | Resolved |
| OpenHands | Resolved | Resolved |
| OpenJiuwen | Resolved; iteration limit | Resolved |
| Claude Code | Resolved | Resolved |
| Goose | Resolved | Resolved |
| Hermes | Resolved; collector failure | Resolved |
| Cline | Resolved | Resolved |
| Continue | Resolved | Resolved |

Pi on `sympy__sympy-13091` ran with completed bpftrace, perf and Perfetto
records, but the official evaluator did not resolve the task; ASB recorded
`workload_check_failed`. Its agent exited zero, execution and cleanup were
confirmed, and evaluation completed. This is an observed unsuccessful task outcome, not
evidence by itself of a collector or evaluator defect. Pi on
`sympy__sympy-14774` completed and resolved its task. The repaired OpenCode execution
on `sympy__sympy-13091` started successfully and its bpftrace, perf and
Perfetto records completed; its agent exited zero and execution, evaluation
and cleanup completed, but the official evaluator did not resolve the task. The second
sweep's OpenCode `sympy__sympy-14774` case completed and resolved the task.
The remaining agents—WorkBuddy, Qwen Code, Mini-SWE, OpenHands, Claude Code,
Goose, Cline and Continue—completed and resolved both tasks. OpenJiuwen and
Hermes completed and resolved `sympy__sympy-14774`; their
`sympy__sympy-13091` exceptions are described below. These are task and run
outcomes, not a claim that every individual measurement succeeded.
All five bpftrace profiles reported completed captures in each of these first
six repaired-sweep records, including the two unresolved tasks.
The repaired Codex `sympy__sympy-13091` and WorkBuddy
`sympy__sympy-13091` run records reported completion and task resolution,
but their individual perf validation records failed at the same decoded-output
limit described below. Their perf measurements are not qualified.
Separate revalidation of copies of those captures with the partial-symbol
candidate decoded 6,375,927 frames for Codex (112,487 unknown) and 4,814,114
for WorkBuddy (131,041 unknown), and generated flamegraphs for both. These
are post-processing checks of the retained data; they do not change the
original failure records. Their symbol trees were partial and inline frames
were omitted.
Continue `sympy__sympy-13091` also hit the old perf decode limit. Separate
revalidation generated a flamegraph, but 3,868,742 of 3,963,151 decoded frames
were unknown, so its symbol coverage remains poor.
The sweep started before PR #1255 merged, so its Hermes token accounting requires
a separate verification with the merged fix.
In the sweep's Hermes `sympy__sympy-13091` attempt, the official evaluator
reported resolution, but perf did not complete its graceful shutdown within
five seconds and symbol custody could not finalize. ASB marked the run as a
collector failure. Its roughly 239 MiB raw perf capture remains incomplete;
the installed perf decoder rejected it because its data-size header was zero.
The separate Hermes follow-up below is a different execution.

A separate OpenJiuwen `sympy__sympy-13091` follow-up with `--max-steps 200`
exited zero and the evaluator reported resolution. Its five bpftrace profiles
and Perfetto record completed, but perf's offline validation exceeded its
256 MiB decoded-output capture limit. ASB reported the overall run as
completed; the perf measurement itself was marked failed and is not qualified.
In a separate test of the retained perf data, the streaming decoder processed
4,892,387 frames but reported 4,809,801 unknown. An experimental verified
partial symbol tree reduced the unknown count to 84,726 of 4,896,651 decoded
frames and produced a flamegraph; eight recorded build-ID images were missing
and inline frames were omitted. This tests post-processing, not a replacement
for the original measurement record or proof of complete symbol coverage.

Separate Hermes follow-ups used the merged token-accounting fix. Both tasks
resolved with agent exit success. The `sympy__sympy-13091` record reported
1,165,597 input, 1,040,512 cached, 8,954 output and 1,174,551 total tokens;
its perf validation hit the same decoded-output limit. The
`sympy__sympy-14774` record reported 925,989 input, 888,448 cached, 8,760
output and 934,749 total tokens, and its perf record completed. Cached tokens
are included in input rather than added again to total.
Separate post-processing of the first Hermes capture generated a flamegraph
with a partial symbol tree, but 11,121,014 of 11,369,231 decoded frames were
unknown. That flamegraph has poor symbol coverage and is not qualified as a
complete representation of agent execution.

PR #1259 replaced bounded in-memory perf decoding with streaming validation;
PR #1263 added verified partial symbol-tree support with explicit limitations.
PR #1270 retries transient guest-symbol copy failures within a bounded budget.
PR #1273 extends the bounded perf shutdown grace period; it is a mitigation,
not proof that the original incomplete recording can be recovered. None of
these changes retroactively alters the original sweep outcomes above.
