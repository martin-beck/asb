# Node26 SymPy sweep attempt

An ASB sweep was prepared for 13 agents against the two original SWE-bench
instances `sympy__sympy-14774` and `sympy__sympy-13091`, using OpenRouter model
`deepseek/deepseek-v4.1-flash` and one agent replica per configuration. The
node26 host was aarch64; the selected task images required x86_64. This attempt
used a temporary x86_64 software-emulated guest and is historical diagnostic
evidence, not a native performance measurement or a completed qualification.

The first full sweep exited with 26 failed benchmarks. Its comparison contained
26 rows covering 13 agents, each reporting missing measurement files; none
contained a benchmark outcome. This does not establish agent execution or
official task resolution. A subsequent focused run identified a project-local
perf executable that could not load its shared libraries. A kernel-matched
guest perf binary passed a task-clock probe after replacement; this observation
does not independently prove the cause of every failed sweep row. ASB's perf
diagnostics were improved in PR #1236.

A separate Perfetto-only pilot failed during pre-agent staging when an `id -g`
helper was reported as timed out despite exit status zero. PR #1241 extended
the separate, bounded helper allowance; its offline tests passed. A replacement
pilot and a second full sweep were started, but were interrupted before a
terminal result at the user's direction to stop VM/emulator execution. Their
outcomes and the effectiveness of that repair on this guest remain unverified.

The guest and its owned workload processes were stopped. A native node26 sweep
of these selected x86_64 workloads remains unperformed; no result from this
attempt should be treated as a native aarch64 benchmark or an agent score.
