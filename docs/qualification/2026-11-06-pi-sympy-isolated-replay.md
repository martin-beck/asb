# Pi SymPy isolated replay qualification, 2026-11-06

In a disposable x86-64 Linux VM, ASB main `81c23d898bff21a96f3f6ef15af5ada30e8d2bcb`
replayed a retained Pi snapshot recording for original SWE-bench task
`sympy__sympy-22236`. The recording used a synthetic provider, one loopback HTTP
tool request and the upstream reference patch; it is not evidence of an agent
solving the task. Both fixture services were stopped before replay. The test
used `crates/asb-cli/tests/fixtures/pi_native/swe_snapshot_c4.py` with
`ASB_TEST_RESUME_RECORDING` pointing to a copy of the retained record evidence.

All four concurrent replay conversations completed, received a successful
official grade, and reported workload cleanup confirmed. Recording and replay
stderr were empty. While the replay clients were running, Docker inspection
showed zero attached networks on each of the four task containers. The separate
upstream grader containers retained their ordinary network configuration.
Absence of a host-gate warning is not a positive quiescence receipt. The
resumed run cannot independently recheck the original recording's HTTP counter.

A separate disposable-container probe used a cached SymPy image and a local
HTTP fixture. A direct host-bridge connection succeeded before disconnection
and failed afterward, while the ASB Docker-exec loopback relay still served the
fixture. It did not contact an external provider. A first VM boot used QEMU's
default CPU model, which lacked the x86-64-v2 features required by the installed
NumPy; all replicas failed during preparation before the test reached agent
execution. The successful qualification used `-cpu host` and a clean main build.

The fixture evidence is retained in the VM under
`/home/asb/qualification/project/pi-swe-network-final-20261106`. This covers
one known recording, agent and task. It does not establish arbitrary native
tool, direct-process, filesystem, network or agent replay completeness.
