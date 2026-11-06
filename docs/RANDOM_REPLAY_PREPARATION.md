# Sampled trial preparation design

Status: implementation in progress. Ordinary sweep replay must remain offline;
the separate `asb sweep SWEEP.toml --prepare-replay` mode is the only proposed
mode here that may make fresh live provider calls to build replacement tapes.

Setup should accept a numerical trial count for each selected workload. It must
publish a sweep plus bounded sampling metadata: the workload, catalog identity,
seed, requested count and candidate order. The count is a number of distinct
trials, not repetitions of a single trial. A stable random permutation permits
replacements without retrying candidates and ensures later runs can use the same
selection. Numeric trial identifiers must remain selectable explicitly.

Preparation operates per agent/provider/model/workload combination. It first
attempts the initially selected trials, records each candidate, and actually
replays it offline. Only a completed recording with successful replay counts
toward the target. If a replay fails, preparation takes the next unused trial
from the same workload, records and verifies that trial. It stops with an
actionable exhaustion error if the catalog cannot supply enough successful
trials; it must not pretend a failed or incomplete recording qualified.

Preparation must persist the selected candidates, attempts, immutable cassette
references and final successful mapping under a sweep-owned lock. It must retain
failed evidence and publish a complete replay selection only after each winner
is verified; interruption must not turn a partially prepared set into success.
On rerun it should validate and reuse published successful cassettes, not record
them again. Catalog drift and changed benchmark inputs need explicit diagnostics.
Multiple agents and models need distinct compatibility-bound recordings, and
parallel preparation must not claim a candidate twice or publish mixed results.

Normal `asb sweep SWEEP.toml --replay` resolves the published selection and
uses exactly its successful trials. It must not contact the live provider, create
new recordings or silently pick replacements. A missing or inconsistent prepared
selection should tell the user to run the preparation mode. Other, unsampled
sweeps keep their current behavior. Reports must identify the actual trials.

Tests need real CLI setup and sweep paths with local fake providers and agent
fixtures: failed first replay followed by successful replacement, independent
workloads and agent/model combinations, exhausted candidates, interrupted
publication, changed inputs, and a second offline replay with the provider
stopped and no additional recordings. Also test count bounds, stable sampling,
numeric trial IDs and legacy unsampled sweeps. CI must not call live providers.
