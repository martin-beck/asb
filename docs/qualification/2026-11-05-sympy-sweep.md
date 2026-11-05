# Local SymPy sweep, 2026-11-05

A live x86-64 Linux sweep used OpenRouter `deepseek/deepseek-v4.1-flash`, one agent replica per configuration, and two randomly selected SWE-bench test instances: `sympy__sympy-14774` and `sympy__sympy-13091`. The dataset revision was `c6fe717fd7a4c3ac1daa4055a4fd082c6a1d28a2`; evaluation used the upstream SWE-bench harness at `02e7a74ffd0b707aab73d203fe87bdc7c76afc8e`. The original executable was built from main commit `8e71c4c45d05c92d61e89dc95a2b42e9b1815ecf`.

The initial sweep attempted all 26 combinations. Twenty-one completed with an official resolved outcome. Both Claude Code attempts failed during guest preparation; both Goose attempts reached an incorrect provider URL and were unresolved; OpenJiuwen's second trial reached its iteration limit, although its submitted patch resolved the task. The five corresponding targeted reruns below each completed and had an official resolved outcome with harness exit code zero.

| Agent | 14774 | 13091 |
| --- | --- | --- |
| Codex | Original pass | Original pass |
| Pi | Original pass | Original pass |
| OpenCode | Original pass | Original pass |
| WorkBuddy | Original pass | Original pass |
| Qwen Code | Original pass | Original pass |
| Mini-SWE | Original pass | Original pass |
| OpenHands | Original pass | Original pass |
| OpenJiuwen | Original pass | Targeted rerun pass |
| Claude Code | Targeted rerun pass | Targeted rerun pass |
| Goose | Targeted rerun pass | Targeted rerun pass |
| Hermes | Original pass | Original pass |
| Cline | Original pass | Original pass |
| Continue | Original pass | Original pass |

The Goose reruns used an executable containing the OpenRouter endpoint fix in PR #1178. The Claude reruns used a frozen executable with the non-root process-observation repair proposed in PR #1179. The OpenJiuwen rerun explicitly selected `--max-steps 65533`; its iteration-limit diagnostic is in PR #1185. These are separate attempts, not replacements for the initial sweep's recorded outcomes.

The original OpenHands runs did not report token usage. A separate patched 14774 run resolved the task and reported 359,836 input, 3,941 output, 334,848 cached and 363,777 total tokens; the guest-usage repair is in PR #1184. OpenCode tool-count reporting was repaired in PR #1176. The compacted continuation-receipt warning observed in the original sweep is addressed by PR #1171. The enabled collector set included process memory and workers, CPU accounting, system, mpstat, iostat, perf, Perfetto, flamegraph, bpftrace and tool-call collection; this record does not independently qualify every collector or claim replay coverage.

The node26 sweep is a separate qualification. Its results are not included in these local counts.
