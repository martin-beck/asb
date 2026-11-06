# Sampled trial preparation design

Status: implementation in progress. Ordinary sweep replay must remain offline;
the separate `asb sweep SWEEP.toml --prepare-replay` mode is the only proposed
mode here that may make fresh live provider calls to build replacement tapes.

Current draft: setup writes sampled metadata and the explicit preparation mode
parses, but sampled sweeps still refuse execution. `prepare_file` now owns a
locked durable `BoundTrialPreparation`; `record_and_replay_candidate` calls the
real runner for live recording and offline replay. An offline fixture combines
them to verify a failed replay, a replacement recording and replay, and reuse of
the successful ledger without extra provider calls. The next step is to connect
these pieces to every sweep combination and publish immutable winning
configuration/cassette mappings only after every requested combination is
complete. Then resolve that published mapping only for ordinary replay. Do not
remove the refusal until both paths are implemented and exercised end to end.

Draft progress: `asb sweep --prepare-replay` now iterates sampled combinations
and records/replays replacements through the locked group ledger. The CLI fixture
checks a mismatch, a replacement, and a second preparation without additional
provider calls. It deliberately reports that selection publication is missing;
ordinary sampled replay is still refused. A completed group can now be reopened
read-only and its cassette/configuration identities revalidated without running a
provider. Remaining work includes publishing a whole-sweep winner mapping,
resolving it from normal replay, preserving original sweep result/report naming,
supporting mixed sampled/fixed workloads and `--project-dir`, and verifying the
complete CLI record/prepare/replay path with the provider stopped.

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
Replay integrity and an agent's task grade are different observations: an
unresolved workload can still have a faithful replay. Qualification must use
typed replay/recording and cleanup evidence, rather than treating a false
workload grade as proof that the tape failed or treating a zero exit as proof
that every recorded interaction was consumed.

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
