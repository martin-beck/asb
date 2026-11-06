# Native ARM source-build probe for SymPy 22236

On 2026-11-06, an isolated `linux/arm64` Docker build on the ARM64 host
node26 used the pinned `swe-bench-tasks` revision
`3d07b464b7b311a0cbfb5ed5b2d8a3b96f84a33d` and its
`sympy__sympy-22236` task definition. The published upstream ARM image for
this task was absent. The probe changed the Dockerfile platform and Miniconda
installer to ARM64, changed the linker package to the ARM variant, and removed
architecture-specific Conda build identifiers while retaining package version
pins. This is a derived environment, not a published upstream ARM image or a
claim of byte-identical dependencies. No VM or CPU emulator was used.

The image reported `aarch64`, Python 3.9.20 and mpmath 1.3.0. With the original
upstream evaluation script, networking disabled and separate fresh containers:

- The unmodified base failed the required `test_Mul`; all 143 expected
  pass-to-pass tests passed.
- The reference patch passed `test_Mul` and all 143 expected pass-to-pass
  tests; the runner reported 145 tests passed in total.

The status counts were checked with the pinned SDK's `parse_log_sympy` parser
against the pinned task's test list. The exact adapted Dockerfile SHA-256 was
`be1b527611901c487564cd37a94d5a9555d5e3fee21c62ee9abfe473af806d65`.
ASB's source-image builder and missing-image resolver then independently built
that same definition on node26 and recorded an ARM64 adaptation receipt.
The owned evidence and logs were retained under
`/tmp/asb-native-arm-verification-H0dV0r/` on node26.

This checks one task's environment and evaluator. It does not prove equivalence
of all architecture-dependent packages, qualify other SWE-bench tasks, or
establish a live agent, native tool recording, or C4 replay result.
