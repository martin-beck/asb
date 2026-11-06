# Node26 Perfetto preflight, 2026-11-06

A disposable x86-64 Ubuntu 24.04 guest ran under QEMU software emulation on the
aarch64 node26 host. The guest had 4 GiB temporary swap configured. This is a
functional check under emulation, not a native performance comparison. The ASB
executable SHA-256 was
`a9b2d0f84f069cb8f118f75037f0e9c73ecaacc566c1100773024b8588d79ae4`.

The pinned Perfetto v58.2 x86 release binaries could not run on this guest
because its virtual CPU lacks AVX2. Local source builds of the same Perfetto
revision, `add693d8b338ba9599dbcbc3e300b1ab8c000897`, with
`enable_perfetto_x64_cpu_opt=false` were installed in the guest project. Their
tracebox and trace-processor SHA-256 values were respectively
`ffa474a2717ba287c1daa535a14cea57c1bf10959bbd9cc451883693c4dc1e6f` and
`c2227c510743b5725e1baa02d349cb81b272fd1cf9c359678635a1bb744b6dcb`.

One live ASB run used Codex, OpenRouter `deepseek/deepseek-v4.1-flash`, the
bundled `bug-fix` workload and the Perfetto collector. The run exited zero;
the structured result reported execution success, a resolved bundled-fixture
evaluation, and confirmed cleanup. It recorded five steps, six tool calls,
109,663 input tokens, 1,147 output tokens, 100,096 cached tokens and 110,810
total tokens. The collector published scheduler metrics and time series.

The 4,249,905-byte trace SHA-256 was
`9e7d0eae2cacbaf6b95b10ce42795392f0c65560c969e861e1d625beace1f24f`;
the guest and copied trace checksums matched. Independently running the frozen
ASB executable's scope SQL against that copy returned zero out-of-tree events;
the checked Perfetto loss counters were zero. These observations concern this
one run and collector, not every agent or collector.

Before the agent started, a native process gate reported exit 101 without a
quiescence receipt. Its cause remains unresolved even though the final
benchmark result reported cleanup confirmed. Earlier direct Tracebox testing
without ASB's private namespace bootstrap could not create an ftrace controller
and is not equivalent to the successful ASB capture. Ubuntu's AppArmor
unprivileged-user-namespace restriction also had to be changed in the disposable
guest; PR #1199 adds a separately approved host setup option.

This preflight is not the requested node26 SWE-bench sweep, which had not yet
started when this record was written.
