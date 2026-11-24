//! Strict immutable per-slot settlement, separate from reporting fallbacks.
use super::super::{Fingerprint, Inputs, Root, completed, resume_error};
use crate::runner::campaign::generation::{self, Catalog};
use crate::runner::campaign::{ExecutionIdentity, Slot, StartIntent, reading::Layout};
use crate::sweep::plan::{PlannedBenchmark, SweepPlan};
use crate::workloads::{contracts::EnvironmentKind, recovery::AttemptReference};
use crate::{Error, io_error};
use completed::{OutcomeBatch, OutcomeContext, OutcomeEvidence};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

const METADATA: u64 = 64 * 1024;
const SCOPE: &str = "settled-campaign-slot-declared-evidence";
const RECEIPT_VERIFIERS: usize = 4;
const RETAINED_OUTCOME_VERIFIERS: usize = 4;

#[path = "recording_recovery.rs"]
mod recording_recovery;

#[path = "continuation.rs"]
pub(in crate::sweep::checkpoint::campaigns) mod continuation;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepared {
    schema_version: u32,
    scope: String,
    compatibility: crate::cassette::Compatibility,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Allocation {
    schema_version: u32,
    scope: String,
    execution: ExecutionIdentity,
    reference: AttemptReference,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    scope: String,
    configuration: usize,
    execution: ExecutionIdentity,
    input_digest: String,
    roots: Vec<Root>,
    fingerprint: Fingerprint,
}

struct Candidate {
    configuration: usize,
    slot: u64,
    execution: ExecutionIdentity,
    roots: Vec<Root>,
    summary: Value,
    receipt_binding: Option<generation::Blob>,
}

// Constructed only after all common declarations and grouped track proofs
// pass. Both sealing and verification derive their authority from this view,
// never from roots or task identities supplied by an existing receipt.
struct Inspection {
    identities: BTreeMap<usize, crate::cassette::Compatibility>,
    layouts: Vec<Layout>,
    candidates: Vec<Candidate>,
    records: Vec<Record>,
    unsettled: Vec<usize>,
}

struct DeclaredInspection {
    identities: BTreeMap<usize, crate::cassette::Compatibility>,
    layouts: Vec<Layout>,
    candidates: Vec<Candidate>,
    records: Vec<Record>,
    unsettled: Vec<usize>,
    proofs: OutcomeBatch,
}

impl DeclaredInspection {
    fn prove(self) -> Result<Inspection, Error> {
        let _validated = self.proofs.verify()?;
        Ok(Inspection {
            identities: self.identities,
            layouts: self.layouts,
            candidates: self.candidates,
            records: self.records,
            unsettled: self.unsettled,
        })
    }
}

struct Record {
    configuration: usize,
    slot: usize,
    path: PathBuf,
    value: Value,
    bytes: u64,
    sha256: String,
}

struct SlotOwner<'a> {
    layout: &'a Layout,
    slot: &'a Slot,
    entry: &'a PlannedBenchmark,
    invocation: &'a Path,
    start: ResolvedStart,
}

struct ResolvedStart {
    relative: PathBuf,
    intent: StartIntent,
    binding: generation::Resolved,
}

// Retained evaluator checks can launch an official workload helper. Keep all
// data needed for that read-only check owned so declarations can be completed
// first, then validated with a bounded amount of parallelism.
struct RetainedValidation {
    project_root: PathBuf,
    project_config: crate::project::ProjectConfig,
    reference: AttemptReference,
    summary: Value,
}

impl RetainedValidation {
    fn verify(
        &self,
        verify: &impl Fn(&crate::workloads::recovery::RetainedOutcomeContext<'_>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        verify(&crate::workloads::recovery::RetainedOutcomeContext {
            recovery: crate::workloads::recovery::RecoveryContext {
                project_root: &self.project_root,
                project_config: &self.project_config,
                reference: &self.reference,
            },
            summary: &self.summary,
        })
    }
}

fn resolve_start(
    layout: &Layout,
    slot: &Slot,
    configuration: usize,
    catalog: Option<&Catalog>,
) -> Result<ResolvedStart, Error> {
    let relative = PathBuf::from(format!("batch-{}/starts/{}.json", slot.batch, slot.replica));
    let intent: StartIntent = serde_json::from_value(layout.declaration(&relative, METADATA)?)
        .map_err(|_| resume_error("campaign start is malformed"))?;
    if intent.schema_version != 1
        || intent.scope != "execution-start-intent-not-actual-attempt"
        || intent.execution != layout.plan.execution(slot)
        || intent
            .execution_cohort
            .as_ref()
            .is_none_or(|cohort| !cohort.admits(slot.replica, layout.plan.concurrency))
    {
        return Err(resume_error(
            "campaign start differs from original slot/cohort",
        ));
    }
    let binding = if let Some(catalog) = catalog {
        catalog.resolve(
            intent.generation.as_ref(),
            configuration,
            &layout.plan,
            slot,
            intent.execution_cohort.as_ref(),
        )?
    } else {
        if intent.generation.is_some() {
            return Err(resume_error("campaign start has no admitted generation"));
        }
        generation::Resolved {
            prepared: PathBuf::from(format!("batch-{}/prepared.json", slot.batch)),
            evidence: Vec::new(),
        }
    };
    Ok(ResolvedStart {
        relative,
        intent,
        binding,
    })
}

impl Inputs {
    /// Only called after execution workers and recording producers have joined.
    /// A missing receipt never changes an independently computed fresh grade.
    pub(in crate::sweep) fn seal_campaigns(
        &self,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
    ) -> Result<(), Error> {
        self.seal_campaigns_with(
            parent,
            results,
            plan,
            crate::workloads::recovery::verify_retained,
        )
    }

    #[cfg(test)]
    pub(in crate::sweep::checkpoint) fn seal_campaigns_fixture(
        &self,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
        verify: impl Fn(&crate::workloads::recovery::RetainedOutcomeContext<'_>) -> Result<(), Error>
        + Sync,
    ) -> Result<(), Error> {
        self.seal_campaigns_with(parent, results, plan, verify)
    }

    fn seal_campaigns_with(
        &self,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
        verify: impl Fn(&crate::workloads::recovery::RetainedOutcomeContext<'_>) -> Result<(), Error>
        + Sync,
    ) -> Result<(), Error> {
        let declarations = self.declare_campaign_layouts(
            parent,
            results,
            plan,
            shared_layouts(results, plan)?,
            verify,
        )?;
        let mut remaining = 256 * 1024 * 1024;
        let mut inherited = BTreeMap::new();
        for candidate in &declarations.candidates {
            let destination =
                self.campaign_completion_path(candidate.configuration, candidate.slot);
            let present = crate::runner::campaign::reading::present_relative(
                &self.directory,
                destination
                    .strip_prefix(&self.directory)
                    .expect("derived receipt path"),
            )?;
            // Reconciliation can create a validated receipt after the original
            // generation catalog was sealed. Treat that direct receipt as
            // inherited only after the same identity and evidence checks used
            // for catalog-bound receipts.
            if candidate.receipt_binding.is_some() || present {
                let (receipt, _) = self.read_campaign_receipt(candidate, &mut remaining)?;
                inherited.insert((candidate.configuration, candidate.slot), receipt);
            }
        }
        let Inspection {
            layouts,
            candidates,
            records,
            unsettled,
            ..
        } = declarations.prove()?;
        // Finish every read-only validation before the first receipt write.
        let inherited_candidates: Vec<_> = candidates
            .iter()
            .filter_map(|candidate| {
                inherited
                    .get(&(candidate.configuration, candidate.slot))
                    .map(|receipt| (candidate, receipt))
            })
            .collect();
        self.verify_campaign_candidates(&inherited_candidates)?;
        let mut newly_completed = Vec::new();
        for candidate in candidates {
            if inherited.contains_key(&(candidate.configuration, candidate.slot)) {
                continue;
            }
            let destination =
                self.campaign_completion_path(candidate.configuration, candidate.slot);
            if crate::runner::campaign::reading::present_relative(
                &self.directory,
                destination
                    .strip_prefix(&self.directory)
                    .expect("derived receipt path"),
            )? {
                return Err(resume_error(
                    "newly admitted slot already has a receipt; explicit recovery is required",
                ));
            }
            let fingerprint = self.snapshotter.fingerprint(
                &candidate.roots,
                &self.campaign_completion_context(&candidate),
                false,
                &[],
            )?;
            newly_completed.push((candidate, fingerprint));
        }
        for (candidate, fingerprint) in newly_completed {
            completed::publish(
                &self.campaign_completion_path(candidate.configuration, candidate.slot),
                &Receipt {
                    schema_version: 1,
                    scope: SCOPE.into(),
                    configuration: candidate.configuration,
                    execution: candidate.execution,
                    input_digest: self.initial.digest.clone(),
                    roots: candidate.roots,
                    fingerprint,
                },
            )?;
        }
        for index in &unsettled {
            let record = &records[*index];
            let layout = &layouts[record.configuration];
            let slot = &layout.plan.slots[record.slot];
            completed::publish(
                &self
                    .directory
                    .join("campaign-unsettled")
                    .join(format!("{:06}/{:06}.json", record.configuration, slot.id)),
                &json!({"schema_version":1,"scope":"unsealed-slot-recovery-required-not-retry-permission",
                    "configuration":record.configuration,"execution":layout.plan.execution(slot),
                    "input_digest":self.initial.digest,"outcome":record.path}),
            )?;
        }
        if unsettled.is_empty() {
            Ok(())
        } else {
            Err(resume_error(
                "some campaign slots require recovery; completed peers were sealed without retry",
            ))
        }
    }

    fn declare_campaign_layouts(
        &self,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
        layouts: Vec<Layout>,
        verify: impl Fn(&crate::workloads::recovery::RetainedOutcomeContext<'_>) -> Result<(), Error>
        + Sync,
    ) -> Result<DeclaredInspection, Error> {
        let catalog = self.read_generation_catalog(results, plan, &layouts)?;
        self.declare_campaign_layouts_with_catalog(
            parent,
            results,
            plan,
            layouts,
            verify,
            catalog.as_ref(),
        )
    }

    pub(in crate::sweep) fn read_generation_catalog(
        &self,
        results: &Path,
        plan: &SweepPlan,
        layouts: &[Layout],
    ) -> Result<Option<Catalog>, Error> {
        self.read_generation_with(results, plan, layouts, Catalog::read)
    }

    fn read_generation_with<T>(
        &self,
        results: &Path,
        plan: &SweepPlan,
        layouts: &[Layout],
        read: impl FnOnce(
            &Path,
            generation::Context<'_>,
            &[generation::Expected<'_>],
        ) -> Result<T, Error>,
    ) -> Result<Option<T>, Error> {
        if !crate::runner::campaign::reading::present_relative(
            &self.directory,
            Path::new(generation::ROOT),
        )? {
            return Ok(None);
        }
        let index = crate::runner::campaign::reading::bounded(
            &results.join("plan.json"),
            crate::sweep::plan::MAX_PLAN_BYTES as u64,
        )?;
        let expected = layouts
            .iter()
            .zip(&plan.entries)
            .map(|(layout, entry)| {
                Ok(generation::Expected {
                    reference: entry
                        .campaign
                        .as_ref()
                        .ok_or_else(|| resume_error("original campaign reference is missing"))?,
                    plan: &layout.plan,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        read(
            &self.directory,
            generation::Context {
                run_id: &plan.run_id,
                input_digest: &self.initial.digest,
                original_index: generation::Blob::of(&index),
            },
            &expected,
        )
        .map(Some)
    }

    fn declare_campaign_layouts_with_catalog(
        &self,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
        layouts: Vec<Layout>,
        verify: impl Fn(&crate::workloads::recovery::RetainedOutcomeContext<'_>) -> Result<(), Error>
        + Sync,
        catalog: Option<&Catalog>,
    ) -> Result<DeclaredInspection, Error> {
        self.verify_campaign_reservations(results, plan)?;
        let mut starts = preflight(&layouts, catalog)?;
        // Receipt population/type/size is admitted before any recording payload
        // proof. Missing receipts are not fabricated by this read-only pass.
        let mut receipt_bytes = 0u64;
        for (configuration, layout) in layouts.iter().enumerate() {
            for slot in &layout.plan.slots {
                let relative = PathBuf::from(format!(
                    "campaign-completed/{configuration:06}/{:06}.json",
                    slot.id
                ));
                match crate::runner::campaign::reading::inspect_relative(
                    &self.directory,
                    &relative,
                )? {
                    Some(bytes) if bytes <= 1024 * 1024 => {
                        receipt_bytes = receipt_bytes
                            .checked_add(bytes)
                            .filter(|bytes| *bytes <= 256 * 1024 * 1024)
                            .ok_or_else(|| {
                                resume_error("campaign receipt metadata exceeds 256 MiB")
                            })?;
                    }
                    None => (),
                    _ => {
                        return Err(resume_error(
                            "campaign receipt is not a bounded regular file",
                        ));
                    }
                }
            }
        }
        let mut records = Vec::new();
        for (configuration, layout) in layouts.iter().enumerate() {
            for (index, slot) in layout.plan.slots.iter().enumerate() {
                if let Some(outcome) = layout.outcome(slot)? {
                    records.push(Record {
                        configuration,
                        slot: index,
                        path: outcome.path,
                        value: outcome.value,
                        bytes: outcome.bytes,
                        sha256: outcome.sha256,
                    });
                }
            }
        }
        // All declarations pass the common strict policy before one grouped
        // recording proof. No report-only fallback can enter this batch.
        let mut declarations = OutcomeBatch::declare(records.iter().map(|record| {
            (
                &record.value,
                OutcomeContext::from(&plan.entries[record.configuration]),
            )
        }))?;
        let mut prepared = BTreeMap::new();
        let mut identities = BTreeMap::new();
        let mut attempts = BTreeSet::new();
        let mut candidates = Vec::new();
        let mut eligible = BTreeSet::new();
        let mut retained = Vec::new();
        for (index, evidence) in declarations.declarations() {
            let record = &records[index];
            let layout = &layouts[record.configuration];
            let slot = &layout.plan.slots[record.slot];
            if (layout.plan.mode == crate::planning::ExecutionMode::Record)
                != evidence.recording.is_some()
            {
                continue;
            }
            if !attempts.insert(evidence.task.attempt_id.clone()) {
                return Err(resume_error("campaign outcomes contain duplicate attempts"));
            }
            let start = starts
                .remove(&(record.configuration, slot.id))
                .ok_or_else(|| resume_error("campaign outcome has no original start"))?;
            let prepared_key = (record.configuration, start.binding.prepared.clone());
            if let std::collections::btree_map::Entry::Vacant(entry) =
                prepared.entry(prepared_key.clone())
            {
                let value: Prepared =
                    serde_json::from_value(layout.declaration(&start.binding.prepared, METADATA)?)
                        .map_err(|_| resume_error("prepared campaign identity is malformed"))?;
                value
                    .compatibility
                    .validate()
                    .map_err(|error| resume_error(&error.to_string()))?;
                if value.schema_version != 1 || value.scope != "verified-prepared-material" {
                    return Err(resume_error(
                        "prepared campaign identity has an unknown protocol",
                    ));
                }
                if let Some(expected) = identities.get(&record.configuration).or_else(|| {
                    (!layout.plan.ignore_compatibility)
                        .then_some(layout.plan.replay_compatibility.as_ref())
                        .flatten()
                }) {
                    expected
                        .require_match(&value.compatibility)
                        .map_err(|error| resume_error(&error.to_string()))?;
                }
                identities
                    .entry(record.configuration)
                    .or_insert_with(|| value.compatibility.clone());
                entry.insert(value.compatibility);
            }
            let compatibility = &prepared[&prepared_key];
            if let Some(recording) = evidence.recording
                && let Some(binding) = &recording.generation
            {
                catalog
                    .ok_or_else(|| resume_error("recording union has no admitted generation"))?
                    .recording(
                        record.configuration,
                        binding,
                        recording,
                        &start.intent,
                        &layout.plan,
                        slot,
                    )?;
            }
            let roots = slot_roots(
                SlotOwner {
                    layout,
                    slot,
                    entry: &plan.entries[record.configuration],
                    invocation: &parent.join(&plan.entries[record.configuration].reference),
                    start,
                },
                &record.path,
                &evidence,
                compatibility,
                &mut retained,
            )?;
            candidates.push(Candidate {
                configuration: record.configuration,
                slot: slot.id,
                execution: layout.plan.execution(slot),
                roots,
                summary: evidence.summary.clone(),
                receipt_binding: catalog
                    .and_then(|catalog| catalog.completed_receipt(record.configuration, slot.id))
                    .cloned(),
            });
            eligible.insert(index);
        }
        // All layout, outcome, allocation, and workspace checks above remain
        // serially ordered. Only independent retained evaluator reads run in
        // parallel, and every one must pass before declaring the batch.
        verify_bounded(&retained, RETAINED_OUTCOME_VERIFIERS, |candidate| {
            candidate.verify(&verify)
        })?;
        declarations.retain_candidates(&eligible);
        let unsettled = declarations.unsettled().to_vec();
        Ok(DeclaredInspection {
            identities,
            layouts,
            candidates,
            records,
            unsettled,
            proofs: declarations,
        })
    }

    fn campaign_completion_path(&self, configuration: usize, slot: u64) -> PathBuf {
        self.directory
            .join("campaign-completed")
            .join(format!("{configuration:06}/{slot:06}.json"))
    }

    fn campaign_completion_context(&self, candidate: &Candidate) -> Value {
        json!({"protocol":"asb-campaign-completion-v1","scope":SCOPE,
            "input_digest":self.initial.digest,"configuration":candidate.configuration,
            "execution":candidate.execution})
    }

    fn read_campaign_receipt(
        &self,
        candidate: &Candidate,
        remaining: &mut u64,
    ) -> Result<(Receipt, (u64, String)), Error> {
        let path = self.campaign_completion_path(candidate.configuration, candidate.slot);
        let bytes = crate::runner::campaign::reading::bounded_relative(
                &self.directory,
                path.strip_prefix(&self.directory).expect("derived receipt path"),
                (*remaining).min(1024 * 1024),
            )
                .map_err(|_| resume_error("campaign receipt is missing or unreadable; explicit recovery is required, not retry"))?;
        *remaining -= bytes.len() as u64;
        if candidate
            .receipt_binding
            .as_ref()
            .is_some_and(|expected| !expected.matches(&bytes))
        {
            return Err(resume_error(
                "completed receipt bytes differ from their admitted generation binding",
            ));
        }
        let receipt: Receipt = serde_json::from_slice(&bytes)
            .map_err(|_| resume_error("campaign completion receipt is malformed"))?;
        if receipt.schema_version != 1
            || receipt.scope != SCOPE
            || receipt.configuration != candidate.configuration
            || receipt.execution != candidate.execution
            || receipt.input_digest != self.initial.digest
            || receipt.roots != candidate.roots
            || !receipt.fingerprint.valid()
        {
            return Err(resume_error(
                "campaign receipt identity or derived evidence roots differ",
            ));
        }
        let identity = (bytes.len() as u64, crate::cassette::digest(&bytes));
        Ok((receipt, identity))
    }

    fn verify_campaign_candidate(
        &self,
        candidate: &Candidate,
        receipt: &Receipt,
    ) -> Result<(), Error> {
        let actual = self.snapshotter.fingerprint(
            &candidate.roots,
            &self.campaign_completion_context(candidate),
            false,
            &[],
        )?;
        if actual != receipt.fingerprint {
            return Err(resume_error("campaign completed evidence changed"));
        }
        Ok(())
    }

    /// Receipt fingerprints are independent immutable reads. Bound their
    /// verification so large resumed sweeps do not serialize every completed
    /// slot before admitting the remaining work.
    fn verify_campaign_candidates(
        &self,
        candidates: &[(&Candidate, &Receipt)],
    ) -> Result<(), Error> {
        verify_bounded(candidates, RECEIPT_VERIFIERS, |(candidate, receipt)| {
            self.verify_campaign_candidate(candidate, receipt)
        })
    }
}

fn verify_bounded<T: Sync>(
    candidates: &[T],
    limit: usize,
    verify: impl Fn(&T) -> Result<(), Error> + Sync,
) -> Result<(), Error> {
    if candidates.is_empty() {
        return Ok(());
    }
    let next = AtomicUsize::new(0);
    let failure = Mutex::new(None);
    let workers = candidates.len().min(limit.max(1));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    if failure.lock().expect("receipt verifier lock").is_some() {
                        return;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(candidate) = candidates.get(index) else {
                        return;
                    };
                    if let Err(error) = verify(candidate) {
                        let mut recorded = failure.lock().expect("receipt verifier lock");
                        if recorded.is_none() {
                            *recorded = Some(error);
                        }
                        return;
                    }
                }
            });
        }
    });
    match failure.into_inner().expect("receipt verifier lock") {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod bounded_verification_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn bounded_verification_checks_every_candidate_without_exceeding_its_limit() {
        let candidates = [0_u8; 9];
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let completed = AtomicUsize::new(0);
        verify_bounded(&candidates, 3, |_| {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
            active.fetch_sub(1, Ordering::SeqCst);
            completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), candidates.len());
        assert!(peak.load(Ordering::SeqCst) <= 3);
    }

    #[test]
    fn bounded_verification_preserves_a_failure() {
        let error = verify_bounded(&[0_u8, 1, 2], 2, |candidate| {
            (*candidate != 1)
                .then_some(())
                .ok_or_else(|| Error::Run("fixture verification failure".into()))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "fixture verification failure");
    }
}

fn shared_layouts(results: &Path, plan: &SweepPlan) -> Result<Vec<Layout>, Error> {
    let mut layouts = plan.read_campaigns(results)?;
    let budget = std::rc::Rc::new(std::cell::Cell::new(256 * 1024 * 1024));
    for layout in &mut layouts {
        layout.share_observation_budget(budget.clone());
    }
    Ok(layouts)
}

fn preflight(
    layouts: &[Layout],
    catalog: Option<&Catalog>,
) -> Result<BTreeMap<(usize, u64), ResolvedStart>, Error> {
    let mut total = 0u64;
    let mut starts = BTreeMap::new();
    for (configuration, layout) in layouts.iter().enumerate() {
        total = total
            .checked_add(layout.preflight()?)
            .ok_or_else(|| resume_error("settlement size overflow"))?;
        let mut leaves = BTreeSet::new();
        for slot in &layout.plan.slots {
            let start_path =
                PathBuf::from(format!("batch-{}/starts/{}.json", slot.batch, slot.replica));
            leaves.insert(start_path.clone());
            if crate::runner::campaign::reading::inspect_relative(&layout.root, &start_path)?
                .is_some()
            {
                let start = resolve_start(layout, slot, configuration, catalog)?;
                leaves.insert(start.binding.prepared.clone());
                starts.insert((configuration, slot.id), start);
            }
            leaves.insert(PathBuf::from(format!(
                "batch-{}/allocations/{}.json",
                slot.batch, slot.replica
            )));
            leaves.insert(PathBuf::from(format!("batch-{}/prepared.json", slot.batch)));
        }
        for leaf in leaves {
            match crate::runner::campaign::reading::inspect_relative(&layout.root, &leaf)? {
                Some(bytes) if bytes <= METADATA => {
                    total = total
                        .checked_add(bytes)
                        .ok_or_else(|| resume_error("settlement size overflow"))?;
                }
                None => (),
                _ => {
                    return Err(resume_error(
                        "settlement declaration is not a bounded regular file",
                    ));
                }
            }
        }
        if total > 256 * 1024 * 1024 {
            return Err(resume_error(
                "aggregate settlement declarations exceed 256 MiB",
            ));
        }
    }
    Ok(starts)
}

fn slot_roots(
    owner: SlotOwner<'_>,
    path: &Path,
    evidence: &OutcomeEvidence<'_>,
    compatibility: &crate::cassette::Compatibility,
    retained: &mut Vec<RetainedValidation>,
) -> Result<Vec<Root>, Error> {
    let SlotOwner {
        layout,
        slot,
        entry,
        invocation,
        start,
    } = owner;
    let structured = crate::workloads::outcome::BenchmarkOutcome::read(evidence.summary)
        .map_err(|_| resume_error("malformed outcome protocol"))?;
    let policy = structured
        .as_ref()
        .map(|outcome| &outcome.policy)
        .cloned()
        .unwrap_or_default();
    let base_protocol =
        crate::runner::replay_protocol_fingerprint(entry.selection.module.as_ref(), &policy)?;
    if compatibility.agent != entry.configuration.agent
        || compatibility.provider != entry.configuration.provider
        || compatibility.model != entry.configuration.model
        || compatibility.workload != evidence.task.workload
        || compatibility.task != evidence.task.task_id
        || compatibility.dataset_sha256
            != crate::runner::dataset_fingerprint(evidence.task.dataset.as_ref())?
        || !normalization_protocol_matches(
            compatibility,
            &base_protocol,
            if layout.plan.mode == crate::planning::ExecutionMode::Replay {
                layout
                    .plan
                    .replay_compatibility
                    .as_ref()
                    .map(|recorded| recorded.normalization_sha256.as_deref())
                    .unwrap_or_else(|| {
                        layout
                            .plan
                            .normalization_policy
                            .as_ref()
                            .map(|policy| policy.sha256.as_str())
                    })
            } else {
                layout
                    .plan
                    .normalization_policy
                    .as_ref()
                    .map(|policy| policy.sha256.as_str())
            },
        )?
    {
        return Err(resume_error(
            "prepared material identity differs from actual configuration/task/protocol",
        ));
    }
    let execution = layout.plan.execution(slot);
    let replica = crate::runner::replica::Identity {
        index: slot.replica,
        total: layout.plan.concurrency,
    };
    let cohort = start
        .intent
        .execution_cohort
        .as_ref()
        .ok_or_else(|| resume_error("campaign start has unknown dispatch provenance"))?;
    if start.intent.schema_version != 1
        || start.intent.scope != "execution-start-intent-not-actual-attempt"
        || start.intent.execution != execution
        || !cohort.admits(slot.replica, layout.plan.concurrency)
        || evidence.schema_version != 2
        || evidence.replica != Some(replica)
        || evidence.summary["execution"]
            != serde_json::to_value(&execution).map_err(|error| resume_error(&error.to_string()))?
        || evidence.summary["execution_cohort"]
            != serde_json::to_value(cohort).map_err(|error| resume_error(&error.to_string()))?
    {
        return Err(resume_error(
            "campaign outcome differs from its original slot/start/cohort",
        ));
    }
    let mut roots = vec![
        Root::path("outcome", path.to_path_buf()),
        Root::path(
            "association",
            layout.root.join(format!(
                "batch-{}/outcomes/{}.json",
                slot.batch, slot.replica
            )),
        ),
        Root::path("start", layout.root.join(&start.relative)),
        Root::path("prepared", layout.root.join(&start.binding.prepared)),
    ];
    for (index, path) in start.binding.evidence.into_iter().enumerate() {
        roots.push(Root::path(format!("generation-{index}"), path));
    }
    let isolated = crate::sweep::isolated_config(&entry.configuration, &layout.root);
    let config = crate::runner::replica::configuration(&isolated, replica)?;
    let workspace = layout.root.join(&slot.workspace);
    let workspace_artifacts_required = workspace_artifacts_required(
        config.retention.compact_completed_attempts,
        evidence.summary,
    );
    let actual: BTreeSet<_> = evidence.outputs.iter().cloned().collect();
    // Older compact-success publication retained the workspace marker while
    // newer publication may omit it. Accept only the exact declared workspace
    // path when immutable completion evidence proves that it was retained.
    let workspace_retained =
        workspace_output_retained(workspace_artifacts_required, &actual, &workspace);
    if workspace_retained
        && !std::fs::symlink_metadata(&workspace).is_ok_and(|metadata| metadata.is_dir())
    {
        return Err(resume_error("campaign workspace is missing or aliased"));
    }
    let runtime = crate::runner::replica::runtime_path(&workspace);
    let mut outputs = BTreeSet::from([layout.root.join(&slot.log)]);
    if workspace_retained {
        outputs.insert(workspace);
    }
    if std::fs::symlink_metadata(&runtime).is_ok() {
        outputs.insert(runtime);
    }
    if let Some(measurements) = layout.measurement_directory(slot) {
        outputs.insert(measurements);
        let measured = layout
            .measurement(slot)?
            .ok_or_else(|| resume_error("campaign measurement is missing"))?;
        if !completed::measurement_matches(&measured, evidence.summary) {
            return Err(resume_error(
                "campaign measurements differ from the graded slot",
            ));
        }
    }
    if actual != outputs || actual.len() != evidence.outputs.len() {
        return Err(resume_error(
            "campaign output roots differ from the saved slot layout",
        ));
    }
    for (index, output) in outputs.into_iter().enumerate() {
        roots.push(Root::path(format!("output-{index}"), output));
    }
    let module = entry
        .selection
        .module
        .as_ref()
        .ok_or_else(|| resume_error("campaign allocation capability is unknown"))?;
    let allocation_path = PathBuf::from(format!(
        "batch-{}/allocations/{}.json",
        slot.batch, slot.replica
    ));
    match std::fs::symlink_metadata(layout.root.join(&allocation_path)) {
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && module.protocol.environment == EnvironmentKind::Native => {}
        Err(error) => return Err(io_error(error)),
        Ok(_) => {
            let allocation: Allocation =
                serde_json::from_value(layout.declaration(&allocation_path, METADATA)?)
                    .map_err(|_| resume_error("campaign allocation is malformed"))?;
            let (project_root, project_config) = crate::project::load(invocation)?;
            allocation.reference.validate_location(&project_root)?;
            if allocation.schema_version != 1
                || allocation.scope != "allocated-workload-reference-not-outcome"
                || allocation.execution != execution
                || !allocation.reference.matches_selection(&entry.selection)
                || !evidence.workload.contains(&allocation.reference.artifacts)
            {
                return Err(resume_error(
                    "campaign allocation differs from original execution/selection/evidence",
                ));
            }
            if workspace_retained {
                let remembered = slot
                    .workspace
                    .join(".asb-attempts")
                    .join(format!("{}.json", allocation.reference.preparation_id));
                let remembered: AttemptReference =
                    serde_json::from_value(layout.declaration(&remembered, METADATA)?)
                        .map_err(|_| resume_error("malformed workspace allocation linkage"))?;
                if remembered != allocation.reference {
                    return Err(resume_error(
                        "campaign workspace lost its original allocation linkage",
                    ));
                }
            }
            retained.push(RetainedValidation {
                project_root,
                project_config,
                reference: allocation.reference,
                summary: evidence.summary.clone(),
            });
            roots.push(Root::path("allocation", layout.root.join(allocation_path)));
        }
    }
    if let Some(recording) = evidence.recording {
        if recording.campaign_root != layout.root
            || (recording.generation.is_none()
                && Some(&recording.track.destination) != layout.plan.recording.as_ref())
        {
            return Err(resume_error(
                "recording proof differs from campaign destination",
            ));
        }
        recording
            .track
            .compatibility
            .require_match(compatibility)
            .map_err(|error| resume_error(&error.to_string()))?;
        roots.push(Root::path(
            "recording-association",
            recording.association_path()?,
        ));
    }
    let extra: BTreeSet<_> = evidence
        .workload
        .iter()
        .chain(evidence.proxy)
        .cloned()
        .collect();
    for (index, path) in extra.into_iter().enumerate() {
        roots.push(Root::path(format!("evidence-{index}"), path));
    }
    Ok(roots)
}

/// A successful compacted attempt intentionally removes only its private
/// workspace before publishing immutable completion evidence. Campaign
/// continuation accepts that receipt while retaining the stricter workspace
/// requirement for failed or retained attempts.
fn compacted_success(compact_enabled: bool, summary: &Value) -> bool {
    compact_enabled
        && summary["process_success"] == true
        && summary["workload_success"] != false
        && summary.get("failure_kind") == Some(&Value::Null)
        && matches!(
            summary["workload_resource_disposition"].as_str(),
            Some("not-required" | "not-created" | "removed")
        )
}

fn workspace_artifacts_required(compact_enabled: bool, summary: &Value) -> bool {
    !compacted_success(compact_enabled, summary)
}

fn workspace_output_retained(
    artifacts_required: bool,
    outputs: &BTreeSet<PathBuf>,
    workspace: &Path,
) -> bool {
    artifacts_required || outputs.contains(workspace)
}

fn normalization_protocol_matches(
    compatibility: &crate::cassette::Compatibility,
    base_protocol: &str,
    normalization: Option<&str>,
) -> Result<bool, Error> {
    let expected =
        crate::runner::replay_protocol_with_normalizer_digest(base_protocol, normalization)?;
    Ok(
        compatibility.normalization_sha256.as_deref() == normalization
            && compatibility.protocol_sha256.as_deref() == Some(expected.as_str()),
    )
}

#[cfg(test)]
mod normalizer_tests {
    use super::*;
    use crate::project::replay_normalization::{FrozenNormalizer, FrozenPolicy, ProfileSelectors};

    #[test]
    fn settlement_checks_both_policy_identity_and_derived_protocol() {
        let source = b"export function normalizeToolResult({text}) { return text; }";
        let descriptor = FrozenPolicy::from_normalizers(vec![FrozenNormalizer {
            id: "fixture".into(),
            source: source.to_vec(),
            sha256: crate::cassette::digest(source),
            selectors: ProfileSelectors {
                workloads: Some(vec!["bug-fix".into()]),
                ..Default::default()
            },
        }])
        .unwrap()
        .descriptor()
        .unwrap()
        .unwrap();
        let base = crate::cassette::digest(b"base protocol");
        let derived =
            crate::runner::replay_protocol_with_normalizer_digest(&base, Some(&descriptor.sha256))
                .unwrap();
        let mut compatibility = crate::cassette::Compatibility {
            agent: "pi".into(),
            provider: "local".into(),
            model: "fixture".into(),
            workload: "bug-fix".into(),
            task: "bug-fix".into(),
            dataset_sha256: None,
            prompt_sha256: crate::cassette::digest(b"prompt"),
            material_sha256: crate::cassette::digest(b"material"),
            protocol_sha256: Some(derived),
            normalization_sha256: Some(descriptor.sha256.clone()),
        };
        assert!(
            normalization_protocol_matches(&compatibility, &base, Some(&descriptor.sha256))
                .unwrap()
        );
        compatibility.normalization_sha256 = None;
        assert!(
            !normalization_protocol_matches(&compatibility, &base, Some(&descriptor.sha256))
                .unwrap()
        );
        compatibility.normalization_sha256 = Some(descriptor.sha256.clone());
        compatibility.protocol_sha256 = Some(base.clone());
        assert!(
            !normalization_protocol_matches(&compatibility, &base, Some(&descriptor.sha256))
                .unwrap()
        );
        compatibility.normalization_sha256 = None;
        assert!(normalization_protocol_matches(&compatibility, &base, None).unwrap());
    }

    #[test]
    fn compacted_success_allows_missing_workspace_but_failures_do_not() {
        let summary = serde_json::json!({
            "process_success": true,
            "workload_success": true,
            "failure_kind": null,
            "workload_resource_disposition": "removed"
        });
        assert!(compacted_success(true, &summary));
        assert!(!workspace_artifacts_required(true, &summary));
        assert!(!compacted_success(false, &summary));
        assert!(workspace_artifacts_required(false, &summary));
        let workspace = PathBuf::from("campaign/batch-1/workspace");
        assert!(!workspace_output_retained(
            false,
            &BTreeSet::new(),
            &workspace
        ));
        assert!(workspace_output_retained(
            false,
            &BTreeSet::from([workspace.clone()]),
            &workspace
        ));

        let mut failed = summary.clone();
        failed["failure_kind"] = serde_json::json!("collector_teardown");
        assert!(!compacted_success(true, &failed));
        assert!(workspace_artifacts_required(true, &failed));

        let mut retained = summary;
        retained["workload_resource_disposition"] = serde_json::json!("stopped-preserved");
        assert!(!compacted_success(true, &retained));
        assert!(workspace_artifacts_required(true, &retained));
    }
}
