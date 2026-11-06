# SWE-bench source-image material identity

A native ARM Pi/SymPy recording on 2026-11-06 completed agent execution and
published a complete cassette, but the agent's patch did not resolve the
instance. Four subsequent replay replicas were rejected before agent execution
because each source-built image had a different immutable ID. The four images'
ordered rootfs layer digests and runtime configurations were equal after
removing the individually verified ASB ownership labels. The task, SDK and
Git-state checks remain part of material identity.

The source-image material contract now binds architecture, OS, ordered rootfs
layers and the complete image runtime configuration after verifying and
excluding only the attempt's `asb.owner`, `asb.attempt` and `asb.recovery`
labels. Changes to other labels, environment, or layers remain significant.
Prebuilt image identity is unchanged. Actual immutable image IDs remain in the
attempt's image evidence and are used for evaluation and resource cleanup.

On node26, a synthetic-provider recording of `sympy__sympy-22236` using the
native ARM source build passed its official reference-patch grade. After its
local provider and HTTP fixture were stopped, four concurrent offline replays
completed, passed official grades, consumed their native tool streams and
confirmed workload cleanup. A second four-replica run replayed the same retained
cassette after the legacy-digest protocol was added, with the same results.
This fixture does not establish a successful live-model solution.

Older source-built cassettes contain only an image-ID-based material digest.
ASB accepts that legacy digest when the current exact image identity matches;
it does not assume that a newly rebuilt image with different ownership labels
is equivalent without recorded proof. The earlier negative-grade live cassette
therefore remains preserved and is not claimed to have passed replay.
