//! Content-bound outcomes, validated separately from declared input stability.
use super::{Fingerprint, Inputs, Root};
use crate::sweep::plan::PlannedBenchmark;
use crate::workloads::{BaselineEvidence, TaskIdentity};
use crate::{Error, io_error};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const LIMIT: u64 = 1024 * 1024;
const SCOPE: &str = "outcome-evidence-only-not-complete-input-closure";
pub(in crate::sweep) mod allocations;
mod reconcile;
pub(super) mod replicas;
pub(in crate::sweep) mod retained;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Outcome {
    schema_version: u32,
    scope: String,
    workload_cleanup_confirmed: bool,
    #[serde(default, deserialize_with = "present_value")]
    workload_resource_disposition: Option<Value>,
    #[serde(default)]
    proxy_cleanup_confirmed: bool,
    summary: Value,
    workload_evidence: Vec<PathBuf>,
    #[serde(default)]
    proxy_evidence: Vec<PathBuf>,
    #[serde(default)]
    capture_evidence: Vec<crate::proxy::capture::CaptureEvidence>,
    #[serde(default)]
    recording_track: Option<crate::runner::recording::Evidence>,
    #[serde(default)]
    replica: Option<crate::runner::replica::Identity>,
    #[serde(default)]
    outputs: Vec<PathBuf>,
}

// Unlike Option<Value>'s default deserializer, retain explicit null as present.
fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// Only this representation may enter completion sealing or continuation.
/// Raw wire records keep compatibility defaults; construction discharges the
/// protocol, identity, grade, cleanup and evidence obligations together.
pub(in crate::sweep) struct ValidatedOutcome {
    schema_version: u32,
    task: TaskIdentity,
    summary: Value,
    replica: Option<crate::runner::replica::Identity>,
    workload_evidence: Vec<PathBuf>,
    proxy_evidence: Vec<PathBuf>,
    outputs: Vec<PathBuf>,
}

// Metadata validity is deliberately not payload proof or a reusable outcome.
struct DeclaredOutcome {
    record: Outcome,
    task: TaskIdentity,
}

/// Strict declarations remain non-reusable until their grouped payload proof.
pub(in crate::sweep) struct OutcomeBatch {
    declarations: Vec<(usize, DeclaredOutcome)>,
    unsettled: Vec<usize>,
}

pub(in crate::sweep) struct OutcomeEvidence<'a> {
    pub schema_version: u32,
    pub task: &'a TaskIdentity,
    pub summary: &'a Value,
    pub replica: Option<crate::runner::replica::Identity>,
    pub workload: &'a [PathBuf],
    pub proxy: &'a [PathBuf],
    pub outputs: &'a [PathBuf],
    pub recording: Option<&'a crate::runner::recording::Evidence>,
}

/// Strict outcome semantics do not depend on a legacy result-directory layout.
/// Campaign slot admission supplies its own execution/cohort/path bindings in
/// addition to this shared configuration and workload-selection contract.
#[derive(Clone, Copy)]
pub(in crate::sweep) struct OutcomeContext<'a> {
    pub configuration: &'a crate::benchmark::BenchmarkConfig,
    pub selection: &'a crate::workloads::TaskSelection,
}

impl<'a> From<&'a PlannedBenchmark> for OutcomeContext<'a> {
    fn from(entry: &'a PlannedBenchmark) -> Self {
        Self {
            configuration: &entry.configuration,
            selection: &entry.selection,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::sweep) struct Completion {
    schema_version: u32,
    scope: String,
    index: usize,
    input_digest: String,
    roots: Vec<Root>,
    fingerprint: Fingerprint,
}

impl Inputs {
    /// Validate all existing groups before permitting reuse or new work. An
    /// incomplete replica group cannot be retried as if its graded members
    /// never happened. Recovery/per-replica continuation is handled separately.
    pub(in crate::sweep) fn resume_outcomes(
        &self,
        parent: &Path,
        results: &Path,
        plan: &crate::sweep::plan::SweepPlan,
    ) -> Result<crate::sweep::resume::State, Error> {
        let mut reused = crate::sweep::resume::State::default();
        for (index, entry) in plan.entries.iter().enumerate() {
            let directory = results.join(crate::sweep::execution::result_name(
                index,
                &parent.join(&entry.reference),
            ));
            match std::fs::symlink_metadata(self.completion_path(index)) {
                Ok(_) => {
                    let (_, summaries) = self.verify_completed(index, &directory, entry)?;
                    for replica in 1..=entry.configuration.concurrency {
                        let identity = crate::runner::replica::Identity {
                            index: replica,
                            total: entry.configuration.concurrency,
                        };
                        if self.has_replica(index, replica)? {
                            self.verify_replica(index, identity, &directory, entry)?;
                        }
                    }
                    reused.groups.insert(index, summaries);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match std::fs::symlink_metadata(&directory) {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                        Err(error) => return Err(io_error(error)),
                        Ok(_) => {
                            let summaries = self
                                .partial_replicas(index, &directory, entry)
                                .map_err(|error| {
                                    invalid(&format!(
                                        "an unsealed or interrupted replica group exists: {error}"
                                    ))
                                })?;
                            if summaries.len() == entry.configuration.concurrency {
                                if !summaries
                                    .values()
                                    .any(|summary| summary["recovered_grade_only"] == true)
                                {
                                    outcome_state(&directory, entry)?;
                                }
                                reused
                                    .groups
                                    .insert(index, summaries.into_values().collect());
                            } else {
                                reused.slots.insert(index, summaries);
                            }
                        }
                    }
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        for (index, summaries) in &mut reused.groups {
            for summary in summaries {
                self.disclose_reconciliation(*index, summary)?;
            }
        }
        for (index, summaries) in &mut reused.slots {
            for summary in summaries.values_mut() {
                self.disclose_reconciliation(*index, summary)?;
            }
        }
        Ok(reused)
    }

    /// A graded false outcome is completed, not a request for another try.
    /// Errors leave the benchmark result intact and cannot create a reusable
    /// receipt. Reuse additionally requires validating the original input seal.
    pub(in crate::sweep) fn completed(
        &self,
        index: usize,
        directory: &Path,
        entry: &PlannedBenchmark,
    ) -> Result<(), Error> {
        let path = self.completion_path(index);
        if std::fs::symlink_metadata(&path).is_ok() {
            // Never replace a prior receipt with a new interpretation of old
            // results. Changed historical evidence must require a fresh sweep.
            self.verify_completed(index, directory, entry)?;
            return Ok(());
        }
        let (roots, _) = outcome_state(directory, entry)?;
        let context = self.completion_context(index, entry);
        let fingerprint = self.snapshotter.fingerprint(&roots, &context, false, &[])?;
        let receipt = Completion {
            schema_version: 1,
            scope: SCOPE.into(),
            index,
            input_digest: self.initial.digest.clone(),
            roots,
            fingerprint,
        };
        publish(&path, &receipt)
    }

    pub(in crate::sweep) fn verify_completed(
        &self,
        index: usize,
        directory: &Path,
        entry: &PlannedBenchmark,
    ) -> Result<(Completion, Vec<Value>), Error> {
        let receipt: Completion = read_private(&self.completion_path(index))?;
        let (roots, summaries) = outcome_state(directory, entry)?;
        if receipt.schema_version != 1
            || receipt.scope != SCOPE
            || receipt.index != index
            || receipt.input_digest != self.initial.digest
            || !receipt.fingerprint.valid()
            || receipt.roots != roots
        {
            return Err(invalid("receipt identity or evidence roots do not match"));
        }
        let actual = self.snapshotter.fingerprint(
            &roots,
            &self.completion_context(index, entry),
            false,
            &[],
        )?;
        if actual != receipt.fingerprint {
            return Err(invalid("completed result or evaluator evidence changed"));
        }
        Ok((receipt, summaries))
    }

    fn completion_context(&self, index: usize, entry: &PlannedBenchmark) -> Value {
        json!({"protocol":"asb-completion-v1", "scope":SCOPE,
            "index":index,"entry":entry,"input_digest":self.initial.digest})
    }

    pub(in crate::sweep) fn completion_path(&self, index: usize) -> PathBuf {
        self.directory
            .join("completed")
            .join(format!("{index:06}.json"))
    }
}

fn outcome_state(
    directory: &Path,
    entry: &PlannedBenchmark,
) -> Result<(Vec<Root>, Vec<Value>), Error> {
    let directory = std::path::absolute(directory).map_err(io_error)?;
    let outcomes = directory.join(".asb-completions");
    let metadata = std::fs::symlink_metadata(&outcomes).map_err(io_error)?;
    if !metadata.is_dir() {
        return Err(invalid("outcome directory is not a real directory"));
    }
    let mut files = Vec::new();
    for item in std::fs::read_dir(&outcomes).map_err(io_error)? {
        let item = item.map_err(io_error)?;
        if files.len() >= crate::benchmark::MAX_CONCURRENCY
            || item.path().extension().is_none_or(|value| value != "json")
        {
            return Err(invalid("outcome population is excessive or incomplete"));
        }
        files.push(item.path());
    }
    files.sort();
    if files.len() != entry.configuration.concurrency {
        return Err(invalid(
            "completed outcome count does not match selected replicas",
        ));
    }
    let config = &entry.configuration;
    let mut attempts = BTreeSet::new();
    let mut summaries = BTreeMap::new();
    let mut evidence = BTreeSet::new();
    let mut proxy_outputs = BTreeSet::new();
    let mut replica_attempts = BTreeMap::new();
    for file in files {
        let record = ValidatedOutcome::read(&file, entry)?;
        let task = record.task;
        if !attempts.insert(task.attempt_id.clone()) {
            return Err(invalid("outcome contains duplicate attempt identities"));
        }
        if let Some(replica) = record.replica
            && replica_attempts
                .insert(replica.index, task.attempt_id.clone())
                .is_some()
        {
            return Err(invalid("outcome contains duplicate replica slots"));
        }
        for path in record.outputs {
            if !path.starts_with(&directory) {
                proxy_outputs.insert(path);
            }
        }
        for path in record.workload_evidence {
            evidence.insert(path);
        }
        for path in record.proxy_evidence {
            proxy_outputs.insert(path);
        }
        summaries.insert(task.attempt_id, record.summary);
    }
    if config
        .measurements
        .as_ref()
        .is_some_and(|settings| settings.enabled)
    {
        let mut measured_attempts = BTreeSet::new();
        for replica in 1..=config.concurrency {
            let name = if config.concurrency == 1 {
                "measurements".into()
            } else {
                format!("measurements-concurrency-{replica}")
            };
            let measured: Value = read_private(&directory.join(name).join("run.json"))?;
            let attempt = measured["task"]["attempt_id"]
                .as_str()
                .ok_or_else(|| invalid("measurement attempt identity is missing"))?;
            let summary = summaries
                .get(attempt)
                .ok_or_else(|| invalid("measurement and outcome identify different attempts"))?;
            if !measured_attempts.insert(attempt.to_owned())
                || replica_attempts
                    .get(&replica)
                    .is_some_and(|expected| expected != attempt)
                || !measurement_matches(&measured, summary)
            {
                return Err(invalid("measurement and outcome disagree on task results"));
            }
        }
    }
    let mut roots = vec![Root::path("configuration-results", directory)];
    for (index, path) in evidence.into_iter().enumerate() {
        roots.push(Root::path(format!("workload-evidence-{index}"), path));
    }
    for (index, path) in proxy_outputs.into_iter().enumerate() {
        roots.push(Root::path(format!("proxy-output-{index}"), path));
    }
    Ok((roots, summaries.into_values().collect()))
}

impl ValidatedOutcome {
    fn read<'a>(path: &Path, expected: impl Into<OutcomeContext<'a>>) -> Result<Self, Error> {
        let record: Outcome = read_private(path)?;
        Self::validate(record, expected)
    }

    fn validate<'a>(
        record: Outcome,
        expected: impl Into<OutcomeContext<'a>>,
    ) -> Result<Self, Error> {
        let declaration = DeclaredOutcome::validate(record, expected)?;
        if let Some(recording) = &declaration.record.recording_track {
            recording
                .track
                .verify(crate::cassette::Limits::default())
                .map_err(|error| invalid(&error.to_string()))?;
        }
        Ok(declaration.finish())
    }
}

impl OutcomeBatch {
    pub(in crate::sweep) fn declare<'a>(
        requests: impl IntoIterator<Item = (&'a Value, OutcomeContext<'a>)>,
    ) -> Result<Self, Error> {
        let mut declarations = Vec::new();
        let mut unsettled = Vec::new();
        let mut counter = DeclarationBytes(0);
        for (index, (value, context)) in requests.into_iter().enumerate() {
            if index >= 65_533 {
                return Err(invalid("outcome batch exceeds selected population bound"));
            }
            let before = counter.0;
            serde_json::to_writer(&mut counter, value)
                .map_err(|_| invalid("outcome declaration bytes exceed 256 MiB"))?;
            if counter.0 - before > LIMIT {
                return Err(invalid("individual outcome declaration exceeds 1 MiB"));
            }
            let record = serde_json::from_value(value.clone())
                .map_err(|_| invalid("malformed declared campaign outcome"))?;
            match DeclaredOutcome::validate(record, context) {
                Ok(declaration) => declarations.push((index, declaration)),
                Err(_) => unsettled.push(index),
            }
        }
        Ok(Self {
            declarations,
            unsettled,
        })
    }

    pub(in crate::sweep) fn unsettled(&self) -> &[usize] {
        &self.unsettled
    }

    /// Additional slot/layout policy can remove declarations from proof and
    /// reuse, never add unchecked declarations or bypass retained proofs.
    pub(in crate::sweep) fn retain_candidates(&mut self, eligible: &BTreeSet<usize>) {
        let Self {
            declarations,
            unsettled,
        } = self;
        declarations.retain(|(index, _)| {
            if eligible.contains(index) {
                true
            } else {
                unsettled.push(*index);
                false
            }
        });
        unsettled.sort_unstable();
    }

    pub(in crate::sweep) fn declarations(
        &self,
    ) -> impl Iterator<Item = (usize, OutcomeEvidence<'_>)> {
        self.declarations
            .iter()
            .map(|(index, outcome)| (*index, outcome.evidence()))
    }

    pub(in crate::sweep) fn verify(self) -> Result<Vec<ValidatedOutcome>, Error> {
        let tracks: Vec<_> = self
            .declarations
            .iter()
            .filter_map(|(_, declaration)| {
                declaration
                    .record
                    .recording_track
                    .as_ref()
                    .map(|recording| recording.track.clone())
            })
            .collect();
        crate::cassette::TrackEvidence::verify_batch(
            &tracks,
            crate::cassette::ProofLimits::default(),
        )
        .map_err(|error| invalid(&error.to_string()))?;
        Ok(self
            .declarations
            .into_iter()
            .map(|(_, declaration)| declaration.finish())
            .collect())
    }
}

struct DeclarationBytes(u64);
impl Write for DeclarationBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .filter(|bytes| *bytes <= 256 * 1024 * 1024)
            .ok_or_else(|| std::io::Error::other("outcome declaration budget exhausted"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl DeclaredOutcome {
    fn evidence(&self) -> OutcomeEvidence<'_> {
        OutcomeEvidence {
            schema_version: self.record.schema_version,
            task: &self.task,
            summary: &self.record.summary,
            replica: self.record.replica,
            workload: &self.record.workload_evidence,
            proxy: &self.record.proxy_evidence,
            outputs: &self.record.outputs,
            recording: self.record.recording_track.as_ref(),
        }
    }
}

impl DeclaredOutcome {
    fn validate<'a>(
        record: Outcome,
        expected: impl Into<OutcomeContext<'a>>,
    ) -> Result<Self, Error> {
        let expected = expected.into();
        record.validate_protocol(&expected)?;
        validate_capture_declarations(&record)?;
        record.validate_cohort()?;
        let task = record.validate_identity(&expected)?;
        let structured = crate::workloads::outcome::BenchmarkOutcome::read(&record.summary)
            .map_err(|_| invalid("malformed structured benchmark outcome"))?;
        record.validate_module_policy(&expected, structured.as_ref())?;
        record.validate_grade(&expected, structured.as_ref())?;
        if let Some(outcome) = &structured {
            record.validate_lineage(outcome, &task)?;
            record.validate_structured_cleanup(outcome)?;
        }
        record.validate_evidence(&expected)?;
        Ok(Self { record, task })
    }

    fn finish(self) -> ValidatedOutcome {
        let Self { record, task } = self;
        ValidatedOutcome {
            schema_version: record.schema_version,
            task,
            summary: record.summary,
            replica: record.replica,
            workload_evidence: record.workload_evidence,
            proxy_evidence: record.proxy_evidence,
            outputs: record.outputs,
        }
    }
}

impl Outcome {
    fn validate_protocol(&self, expected: &OutcomeContext<'_>) -> Result<(), Error> {
        if !matches!(self.schema_version, 1 | 2)
            || self.scope != "task-outcome-not-yet-a-resume-receipt"
            || self.summary["schema_version"] != 2
        {
            return Err(invalid("outcome protocol or summary schema is unsupported"));
        }
        // Schema 1 predates stable replica slots and remains group-readable.
        if self.schema_version == 2
            && self.replica.is_none_or(|replica| {
                !replica.valid() || replica.total != expected.configuration.concurrency
            })
        {
            return Err(invalid(
                "outcome replica does not match the selected population",
            ));
        }
        if !self.workload_cleanup_confirmed || !self.proxy_cleanup_confirmed {
            return Err(invalid("outcome workload or proxy cleanup is unconfirmed"));
        }
        if !crate::workloads::ResourceDisposition::matches_record(
            self.workload_resource_disposition.as_ref(),
            self.workload_cleanup_confirmed,
            &self.summary,
        ) {
            return Err(invalid(
                "outcome resource disposition contradicts cleanup confirmation",
            ));
        }
        Ok(())
    }

    fn validate_cohort(&self) -> Result<(), Error> {
        let summary = &self.summary;
        if !summary["execution_cohort"].is_null() {
            let cohort: crate::measurements::ExecutionCohort =
                serde_json::from_value(summary["execution_cohort"].clone())
                    .map_err(|_| invalid("outcome execution cohort is malformed"))?;
            let replica = self
                .replica
                .ok_or_else(|| invalid("outcome cohort has no replica identity"))?;
            if summary["replica_index"].as_u64() != Some(replica.index as u64)
                || !cohort.admits(replica.index, replica.total)
            {
                return Err(invalid(
                    "outcome cohort does not match its selected replica",
                ));
            }
            crate::planning::validate_recorded_dispatch(
                summary,
                &cohort,
                replica.index,
                replica.total,
            )?;
        } else if !summary["replica_index"].is_null() {
            return Err(invalid("outcome replica has no execution cohort"));
        }
        Ok(())
    }

    fn validate_identity(&self, expected: &OutcomeContext<'_>) -> Result<TaskIdentity, Error> {
        let summary = &self.summary;
        let config = expected.configuration;
        let task: TaskIdentity = serde_json::from_value(summary["task"].clone())
            .map_err(|_| invalid("outcome task identity is missing or malformed"))?;
        if !expected.selection.matches(&task)
            || task.attempt_id.is_empty()
            || task.attempt_id.len() > 256
        {
            return Err(invalid(
                "outcome task, dataset or attempt differs from its selection",
            ));
        }
        if summary["agent"] != config.agent
            || summary["workload"] != config.workload
            || summary["provider"] != config.provider
            || summary["model"] != config.model
        {
            return Err(invalid("outcome configuration identity does not match"));
        }
        Ok(task)
    }

    fn validate_module_policy(
        &self,
        expected: &OutcomeContext<'_>,
        structured: Option<&crate::workloads::outcome::BenchmarkOutcome>,
    ) -> Result<(), Error> {
        if let Some(outcome) = structured
            && expected.selection.module.as_ref().map_or_else(
                || outcome.policy != crate::workloads::outcome::MetricPolicy::default(),
                |module| !outcome.policy.matches_module(module),
            )
        {
            return Err(invalid(
                "outcome metric policy differs from selected protocol",
            ));
        }
        if !self.summary["module"].is_null() {
            let module: crate::workloads::contracts::ModuleDescriptor =
                serde_json::from_value(self.summary["module"].clone())
                    .map_err(|_| invalid("malformed recorded module"))?;
            if !module.valid()
                || expected.selection.module.as_ref() != Some(&module)
                || structured.is_some_and(|outcome| !outcome.policy.matches_module(&module))
            {
                return Err(invalid("recorded module differs from selected protocol"));
            }
        }
        Ok(())
    }

    fn validate_grade(
        &self,
        expected: &OutcomeContext<'_>,
        structured: Option<&crate::workloads::outcome::BenchmarkOutcome>,
    ) -> Result<(), Error> {
        let summary = &self.summary;
        let baseline: BaselineEvidence =
            serde_json::from_value(summary["workload_baseline"].clone())
                .map_err(|_| invalid("outcome baseline evidence is missing or malformed"))?;
        // Legacy outcomes require a Boolean grade; a completed native reward
        // instead carries its policy's numeric grade with nullable resolution.
        let completed_grade = structured.map_or_else(
            || summary["workload_success"].as_bool().is_some(),
            |outcome| outcome.evaluation.completed(),
        );
        // An evaluator can fail only after a bounded execution has been
        // stopped and its owned resources have been cleaned up.  That is a
        // terminal *ungraded failure*, not a successful grade, but it must be
        // able to settle its own replica: otherwise a `--continue` campaign
        // cannot advance past one preserved failure to independent slots.
        // Keep this deliberately narrower than an arbitrary unavailable
        // result: it requires the structured error to agree with a recorded
        // failed execution.
        let terminal_ungraded_failure = structured.is_some_and(|outcome| {
            matches!(
                outcome.evaluation,
                crate::workloads::outcome::EvaluationOutcome::Error { .. }
            ) && outcome.execution_success == Some(false)
                && summary["process_success"].as_bool() == Some(false)
        });
        if !(completed_grade || terminal_ungraded_failure)
            || summary["process_success"].as_bool().is_none()
            || summary["workload_evaluation"] != expected.selection.evaluation_kind
            || !baseline.supports(&expected.selection.evaluation_kind)
        {
            return Err(invalid(
                "outcome grade, evaluator or baseline is incomplete",
            ));
        }
        Ok(())
    }

    fn validate_lineage(
        &self,
        outcome: &crate::workloads::outcome::BenchmarkOutcome,
        task: &TaskIdentity,
    ) -> Result<(), Error> {
        if outcome.lineage.execution_attempt != task.attempt_id
            || outcome.execution_success != self.summary["process_success"].as_bool()
        {
            return Err(invalid(
                "structured outcome execution lineage does not match its task",
            ));
        }
        let resolution = &self.summary["workload_success"];
        if (!resolution.is_null() && !resolution.is_boolean())
            || outcome.evaluation.resolution() != resolution.as_bool()
        {
            return Err(invalid(
                "structured outcome resolution differs from its summary",
            ));
        }
        Ok(())
    }

    fn validate_structured_cleanup(
        &self,
        outcome: &crate::workloads::outcome::BenchmarkOutcome,
    ) -> Result<(), Error> {
        if outcome.cleanup_confirmed != Some(true) {
            return Err(invalid("structured outcome cleanup is unconfirmed"));
        }
        let declared: BTreeSet<_> = self.workload_evidence.iter().collect();
        let evaluated: BTreeSet<_> = outcome.evidence.iter().collect();
        if evaluated != declared {
            return Err(invalid(
                "structured outcome evidence differs from declared evaluator roots",
            ));
        }
        Ok(())
    }

    fn validate_evidence(&self, expected: &OutcomeContext<'_>) -> Result<(), Error> {
        if self.workload_evidence.len() > 64
            || self.proxy_evidence.len() > 64
            || self.outputs.len() > 64
        {
            return Err(invalid("outcome evidence population exceeds its bound"));
        }
        if expected.selection.dataset.is_some() && self.workload_evidence.is_empty() {
            return Err(invalid("outcome dataset has no evaluator evidence"));
        }
        if self.outputs.iter().any(|path| !path.is_absolute()) {
            return Err(invalid("declared execution output path is relative"));
        }
        for path in &self.workload_evidence {
            if !path.is_absolute() || !path.exists() {
                return Err(invalid(
                    "declared evaluator evidence is missing or relative",
                ));
            }
        }
        for path in &self.proxy_evidence {
            if !path.is_absolute() {
                return Err(invalid("declared proxy output path is relative"));
            }
            // A failed agent may have made no calls and produced no recording.
            // Bind that absence too, rather than retrying a graded false task.
        }
        Ok(())
    }
}

fn validate_capture_declarations(record: &Outcome) -> Result<(), Error> {
    if record.capture_evidence.len() > 64 {
        return Err(invalid("capture evidence population exceeds its bound"));
    }
    if let Some(recording) = &record.recording_track {
        if record.summary.get("recording")
            != Some(
                &serde_json::to_value(recording)
                    .map_err(|_| invalid("cannot encode recording evidence"))?,
            )
        {
            return Err(invalid("recording evidence differs from execution summary"));
        }
        recording.validate_summary_metadata(&record.summary, &record.capture_evidence)?;
    } else if !record.summary["recording"].is_null() {
        return Err(invalid("execution recording has no typed track evidence"));
    }
    for capture in &record.capture_evidence {
        capture
            .validate()
            .map_err(|_| invalid("capture evidence metadata is malformed"))?;
        if !record.proxy_evidence.contains(&capture.output)
            && record
                .recording_track
                .as_ref()
                .is_none_or(|recording| capture.output != recording.producer)
        {
            return Err(invalid(
                "capture output is absent from declared proxy evidence",
            ));
        }
    }
    Ok(())
}

pub(in crate::sweep) fn measurement_matches(measured: &Value, summary: &Value) -> bool {
    [
        "execution_plan_fingerprint",
        "execution",
        "recording",
        "module",
        "benchmark_outcome",
        "replica_index",
        "execution_cohort",
        "schema_version",
        "task",
        "agent",
        "workload",
        "provider",
        "model",
        "process_success",
        "workload_success",
        "workload_evaluation",
        "workload_baseline",
        "workload_baseline_passed",
        "success_rate",
        "token_usage",
        "elapsed_ms",
        "steps",
        "failure_kind",
        "failure_reason",
        "ignored_failures",
        "workload_failure",
        "adapter_smoke_success",
        "workload_resource_disposition",
        "workload_artifacts",
    ]
    .iter()
    .all(|field| {
        if matches!(
            *field,
            "workload_resource_disposition" | "workload_artifacts"
        ) {
            measured.get(*field) == summary.get(*field)
        } else {
            // Older JSON summaries emitted null for optional execution identity
            // while measurements omitted it. Neither representation asserts a
            // campaign or recording. Nonnull identities must still agree exactly;
            // campaign readers additionally require the planned typed identity.
            measured[*field] == summary[*field]
        }
    }) && measured["approximate_replay"].as_bool().unwrap_or(false)
        == summary["approximate_replay"].as_bool().unwrap_or(false)
        && ["planning", "effective_budgets"]
            .iter()
            .all(|field| crate::planning::recorded_object(measured, Some(summary), field).is_ok())
}

fn invalid(reason: &str) -> Error {
    Error::Input(format!(
        "completed task cannot be reused: {reason}; preserve these results and start a fresh sweep"
    ))
}

pub(in crate::sweep) fn read_private<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<T, Error> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.len() > LIMIT {
        return Err(invalid("outcome record is not a bounded regular file"));
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid("outcome record exceeds its size limit"));
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid("outcome record is malformed"))
}

pub(in crate::sweep) fn publish(path: &Path, receipt: &impl Serialize) -> Result<(), Error> {
    let directory = path.parent().expect("completion receipt parent");
    if let Ok(metadata) = std::fs::symlink_metadata(directory)
        && !metadata.is_dir()
    {
        return Err(invalid("receipt directory is not a real directory"));
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(directory).map_err(io_error)?;
    let content = serde_json::to_vec_pretty(receipt)
        .map_err(|_| invalid("cannot encode completion receipt"))?;
    if content.len() as u64 > LIMIT {
        return Err(invalid("completion receipt exceeds its size limit"));
    }
    let temporary = directory.join(format!(
        ".{}.tmp",
        TaskIdentity::local("completion-seal", None).attempt_id
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(io_error)?;
    let result = file
        .write_all(&content)
        .and_then(|()| file.sync_all())
        // Publish without ever replacing an existing historical receipt.
        .and_then(|()| std::fs::hard_link(&temporary, path));
    let cleanup = std::fs::remove_file(&temporary);
    result.and(cleanup).map_err(io_error)?;
    #[cfg(unix)]
    std::fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(io_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn batch_declarations_preserve_false_and_nullable_grades_without_unchecked_outcomes() {
        for fixture in [Fixture::new(false), Fixture::new(true), Fixture::native()] {
            let values = [fixture.read(0), fixture.read(1)];
            let requests = || {
                values
                    .iter()
                    .map(|value| (value, OutcomeContext::from(&fixture.entry)))
            };
            let batch = OutcomeBatch::declare(requests()).unwrap();
            assert_eq!(batch.declarations().count(), 2);
            let validated = batch.verify().unwrap();
            for (outcome, value) in validated.iter().zip(&values) {
                assert_eq!(outcome.summary, value["summary"]);
            }
            let mut broken = values.clone();
            broken[1]["proxy_cleanup_confirmed"] = json!(false);
            let partial = OutcomeBatch::declare(
                broken
                    .iter()
                    .map(|value| (value, OutcomeContext::from(&fixture.entry))),
            )
            .unwrap();
            assert_eq!(partial.unsettled(), &[1]);
            assert_eq!(partial.verify().unwrap().len(), 1);
        }
    }

    #[test]
    fn measurement_agreement_preserves_legacy_nulls_but_binds_campaign_identities() {
        let empty = json!({});
        for field in ["execution", "recording"] {
            let mut changed = empty.clone();
            changed[field] = Value::Null;
            assert!(measurement_matches(&changed, &empty));
            assert!(measurement_matches(&empty, &changed));
            assert!(measurement_matches(&changed, &changed));
            let mut other = changed.clone();
            other[field] = json!({"changed": true});
            assert!(!measurement_matches(&changed, &other));
            assert!(!measurement_matches(&other, &changed));
            assert!(!measurement_matches(&empty, &other));
            assert!(!measurement_matches(&other, &empty));
            assert!(measurement_matches(&other, &other));
            let mut conflict = other.clone();
            conflict[field] = json!({"changed": false});
            assert!(!measurement_matches(&other, &conflict));
        }
    }

    #[test]
    fn direct_outcome_context_preserves_strict_legacy_and_native_semantics() {
        for kind in ["legacy-false", "legacy-true", "native"] {
            let fixture = if kind == "native" {
                Fixture::native()
            } else {
                Fixture::new(kind == "legacy-true")
            };
            let original = fixture.read(0);
            let expected = super::OutcomeContext {
                configuration: &fixture.entry.configuration,
                selection: &fixture.entry.selection,
            };
            for case in [
                "original",
                "schema-two",
                "unknown-grade",
                "wrong-model",
                "wrong-task",
                "missing-baseline",
                "unconfirmed-cleanup",
                "unknown-disposition",
                "missing-dataset-evidence",
                "undeclared-capture",
                "foreign-module",
                "foreign-lineage",
            ] {
                let mut record = original.clone();
                match case {
                    "original" => (),
                    "schema-two" => {
                        record["schema_version"] = json!(2);
                        record["replica"] = json!({"index":1,"total":2});
                    }
                    "unknown-grade" => {
                        if kind == "native" {
                            record["summary"]["benchmark_outcome"]["evaluation"] =
                                json!({"status":"unavailable","reason":"fixture missing grade"});
                        } else {
                            record["summary"]["workload_success"] = Value::Null;
                        }
                    }
                    "wrong-model" => record["summary"]["model"] = json!("foreign"),
                    "wrong-task" => record["summary"]["task"]["task_id"] = json!("foreign"),
                    "missing-baseline" => record["summary"]["workload_baseline"] = Value::Null,
                    "unconfirmed-cleanup" => record["proxy_cleanup_confirmed"] = json!(false),
                    "unknown-disposition" => {
                        record["workload_resource_disposition"] = json!("unknown")
                    }
                    "missing-dataset-evidence" => record["workload_evidence"] = json!([]),
                    "undeclared-capture" => {
                        record["capture_evidence"] = json!([{
                            "adapter":"llmtape","output":fixture.root.join("foreign.jsonl"),
                            "complete":true,"accepted":1,"completed":1,"failed":0,"recorded":1,
                            "inference_requests":1,"bytes":3,"sha256":"a".repeat(64)
                        }]);
                    }
                    "foreign-module" => record["summary"]["module"] = json!({"unknown":true}),
                    "foreign-lineage" => {
                        // A native outcome binds its evaluation to this attempt.
                        // Legacy summaries do not carry that protocol.
                        if kind != "native" {
                            continue;
                        }
                        record["summary"]["benchmark_outcome"]["lineage"]["execution_attempt"] =
                            json!("foreign");
                    }
                    _ => unreachable!(),
                }
                let entry_result = super::ValidatedOutcome::validate(
                    serde_json::from_value(record.clone()).unwrap(),
                    &fixture.entry,
                );
                let context_result = super::ValidatedOutcome::validate(
                    serde_json::from_value(record).unwrap(),
                    expected,
                );
                let accepted = matches!(case, "original" | "schema-two");
                assert_eq!(entry_result.is_ok(), accepted, "{kind}/{case}");
                assert_eq!(context_result.is_ok(), accepted, "{kind}/{case}");
                match (entry_result, context_result) {
                    (Ok(entry), Ok(context)) => {
                        assert_eq!(entry.summary, context.summary);
                        assert_eq!(entry.replica, context.replica);
                        assert_eq!(entry.task, context.task);
                    }
                    (Err(entry), Err(context)) => {
                        assert_eq!(entry.to_string(), context.to_string())
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    #[test]
    fn cleaned_up_execution_failure_with_evaluator_error_can_settle_a_replica() {
        let fixture = Fixture::new(false);
        for index in 0..2 {
            let mut record = fixture.read(index);
            record["summary"]["workload_success"] = Value::Null;
            record["summary"]["process_success"] = json!(false);
            record["summary"]["benchmark_outcome"] = json!({
                "schema_version":1,
                "execution_success":false,
                "evaluation":{"status":"error","reason":"bounded evaluator did not return a task grade"},
                "cleanup_confirmed":true,
                "evidence":[fixture.evidence],
                "policy":crate::workloads::outcome::MetricPolicy::default(),
                "lineage":{"execution_attempt":record["summary"]["task"]["attempt_id"],
                    "evaluation_id":format!("terminal-failure-{index}"),"supersedes":null}
            });
            fixture.write(index, &record);
        }
        fixture.seal().unwrap();
        let (_, summaries) = fixture.verify().unwrap();
        assert!(
            summaries
                .iter()
                .all(|summary| summary["process_success"] == false
                    && summary["workload_success"].is_null())
        );

        let mut invalid = fixture.read(0);
        invalid["summary"]["process_success"] = json!(true);
        invalid["summary"]["benchmark_outcome"]["execution_success"] = json!(true);
        fixture.write(0, &invalid);
        assert!(ValidatedOutcome::read(&fixture.outcome_path(0), &fixture.entry).is_err());
    }

    #[test]
    fn capture_metadata_is_optional_but_validated_before_outcome_reuse() {
        let fixture = Fixture::new(true);
        let original = fixture.read(0);
        assert!(
            super::ValidatedOutcome::validate(
                serde_json::from_value(original.clone()).unwrap(),
                &fixture.entry
            )
            .is_ok()
        );
        let output = fixture.directory.join("capture.jsonl");
        let capture = serde_json::json!({"adapter":"llmtape","output":output,"complete":true,
            "accepted":1,"completed":1,"failed":0,"recorded":1,"inference_requests":1,
            "bytes":3,"sha256":"a".repeat(64)});
        let mut record = original.clone();
        record["capture_evidence"] = serde_json::json!([capture]);
        let validate = |value: serde_json::Value| {
            super::ValidatedOutcome::validate(
                serde_json::from_value(value).unwrap(),
                &fixture.entry,
            )
            .map(|_| ())
        };
        assert!(
            validate(record.clone())
                .unwrap_err()
                .to_string()
                .contains("absent from declared proxy evidence")
        );
        record["proxy_evidence"] = serde_json::json!([output]);
        validate(record.clone()).unwrap();
        for field in ["accepted", "completed", "recorded", "inference_requests"] {
            let mut bad = record.clone();
            bad["capture_evidence"][0][field] = serde_json::json!(99);
            assert!(validate(bad).is_err(), "accepted contradictory {field}");
        }
        record["capture_evidence"] = serde_json::json!(vec![capture; 65]);
        assert!(
            validate(record)
                .unwrap_err()
                .to_string()
                .contains("population exceeds")
        );
    }

    #[test]
    fn settlement_presence_and_measurement_agreement_are_not_lost() {
        #[derive(serde::Deserialize)]
        struct Field {
            #[serde(default, deserialize_with = "super::present_value")]
            value: Option<serde_json::Value>,
        }
        assert!(
            serde_json::from_value::<Field>(serde_json::json!({}))
                .unwrap()
                .value
                .is_none()
        );
        assert_eq!(
            serde_json::from_value::<Field>(serde_json::json!({"value":null}))
                .unwrap()
                .value,
            Some(serde_json::Value::Null)
        );
        let summary = serde_json::json!({"workload_resource_disposition":"stopped-preserved",
            "workload_artifacts":["/owned/attempt"]});
        assert!(super::measurement_matches(&summary, &summary));
        assert!(super::measurement_matches(
            &serde_json::json!({}),
            &serde_json::json!({})
        ));
        for field in ["workload_resource_disposition", "workload_artifacts"] {
            let mut measured = summary.clone();
            measured.as_object_mut().unwrap().remove(field);
            assert!(!super::measurement_matches(&measured, &summary));
            measured[field] = serde_json::Value::Null;
            assert!(!super::measurement_matches(&measured, &summary));
        }
        let mut measured = summary.clone();
        measured["workload_resource_disposition"] = serde_json::json!("removed");
        assert!(!super::measurement_matches(&measured, &summary));
    }
    use super::super::Snapshotter;
    use super::*;
    use crate::workloads::TaskSelection;

    pub(super) struct Fixture {
        pub(super) root: PathBuf,
        pub(super) directory: PathBuf,
        pub(super) evidence: PathBuf,
        pub(super) inputs: Inputs,
        pub(super) entry: PlannedBenchmark,
    }

    #[test]
    fn continuation_distinguishes_untouched_tasks_from_unsealed_replica_groups() {
        let fixture = Fixture::new(false);
        let plan: crate::sweep::plan::SweepPlan = serde_json::from_value(json!({
            "schema_version":1,"run_id":"abc","execution_mode":"live",
            "entries":[&fixture.entry]
        }))
        .unwrap();
        let results = fixture.root.join("sweep-results");
        std::fs::create_dir(&results).unwrap();
        assert!(
            fixture
                .inputs
                .resume_outcomes(&fixture.root, &results, &plan)
                .unwrap()
                .has_no_saved_outcomes()
        );
        let directory = results.join(crate::sweep::execution::result_name(
            0,
            &fixture.root.join(&fixture.entry.reference),
        ));
        std::fs::create_dir(&directory).unwrap();
        let marker = directory.join("existing-work");
        std::fs::write(&marker, "must be preserved").unwrap();
        let error = fixture
            .inputs
            .resume_outcomes(&fixture.root, &results, &plan)
            .unwrap_err();
        assert!(error.to_string().contains("interrupted replica group"));
        assert_eq!(
            std::fs::read_to_string(marker).unwrap(),
            "must be preserved"
        );
    }

    impl Fixture {
        pub(super) fn new(resolved: bool) -> Self {
            let root = std::env::temp_dir()
                .join(TaskIdentity::local("completion-seal-test", None).attempt_id);
            let directory = root.join("results");
            let evidence = root.join("evaluator");
            std::fs::create_dir_all(directory.join(".asb-completions")).unwrap();
            std::fs::create_dir_all(&evidence).unwrap();
            std::fs::write(evidence.join("grade.json"), "private evaluator sentinel").unwrap();
            std::fs::write(directory.join("run.log"), "private transcript sentinel").unwrap();
            let state = root.join("state");
            let input = root.join("input");
            std::fs::write(&input, "unchanged declared input").unwrap();
            let snapshotter = Snapshotter::open(&state, "python3".into()).unwrap();
            let roots = vec![Root::path("input", input)];
            let context = json!({"scope":"fixture"});
            let initial = snapshotter
                .fingerprint(&roots, &context, false, &[])
                .unwrap();
            let inputs = Inputs {
                directory: state,
                snapshotter,
                roots,
                context,
                initial,
                runtime: None,
            };
            let configuration = toml::from_str("agent='fixture-agent'\nprovider='fixture-provider'\nmodel='fixture-model'\nworkload='fixture-suite'\ntrial='case-1'\nconcurrency=2\n").unwrap();
            let selection = TaskSelection {
                module: None,
                workload: "fixture-suite".into(),
                task_id: "case-1".into(),
                dataset: Some(
                    serde_json::from_value(json!({
                        "source":"fixture/dataset", "revision":"pinned", "split":"test"
                    }))
                    .unwrap(),
                ),
                evaluation_kind: "upstream-harness".into(),
                population_size: Some(2),
            };
            let entry = PlannedBenchmark {
                campaign: None,
                policy: None,
                reference: "benchmark.toml".into(),
                configuration,
                selection,
            };
            let fixture = Self {
                root,
                directory,
                evidence,
                inputs,
                entry,
            };
            for index in 0..2 {
                let mut task = TaskIdentity::local("fixture-suite", Some("case-1"));
                task.dataset = fixture.entry.selection.dataset.clone();
                let outcome = json!({"schema_version":1,"scope":"task-outcome-not-yet-a-resume-receipt",
                    "workload_cleanup_confirmed":true,"workload_evidence":[fixture.evidence],
                    "proxy_cleanup_confirmed":true,"proxy_evidence":[],
                    "summary":{"schema_version":2,"agent":"fixture-agent","provider":"fixture-provider",
                        "model":"fixture-model","workload":"fixture-suite","task":task,
                        "workload_evaluation":"upstream-harness","workload_success":resolved,
                            "process_success":true,
                        "workload_baseline":{"kind":"upstream-task-definition","base_commit":"a".repeat(40)}}});
                fixture.write(index, &outcome);
            }
            fixture
        }

        fn native() -> Self {
            let mut fixture = Self::new(false);
            let mut module = crate::workloads::contracts::ModuleDescriptor::local(
                "fixture-suite",
                crate::workloads::KnownEvaluationKind::Ungraded,
            );
            // Historical/native protocol extensions remain unrestricted wire values.
            module.protocol.id = "fixture-grader".into();
            module.protocol.score_policy = "metric:accuracy@1".into();
            fixture.entry.selection.module = Some(module.clone());
            fixture.entry.selection.dataset = None;
            fixture.entry.selection.population_size = None;
            fixture.entry.selection.evaluation_kind = "fixture-grader".into();
            assert!(fixture.entry.selection.valid());
            for index in 0..2 {
                let mut record = fixture.read(index);
                record["schema_version"] = json!(2);
                record["replica"] = json!({"index":index + 1,"total":2});
                record["summary"]["module"] = json!(module);
                record["summary"]["task"]["dataset"] = Value::Null;
                record["summary"]["workload_success"] = Value::Null;
                record["summary"]["workload_evaluation"] = json!("fixture-grader");
                record["summary"]["workload_baseline"] = json!({"kind":"measured","passed":false});
                record["summary"]["benchmark_outcome"] = json!({
                    "schema_version":1,"execution_success":true,
                    "evaluation":{"status":"completed","resolution":null,"rewards":{"accuracy":0.5}},
                    "cleanup_confirmed":true,"evidence":[fixture.evidence],
                    "policy":{"id":"accuracy","version":1,"reward":"accuracy","aggregation":"attempt-mean","missing_rewards":"exclude",
                        "rewards":{"accuracy":{"unit":"ratio","range":[0,1]},"secondary":{"unit":"ratio","range":[0,1]}}},
                    "lineage":{"execution_attempt":record["summary"]["task"]["attempt_id"],"evaluation_id":format!("eval-fixture-{index}"),"supersedes":null}
                });
                fixture.write(index, &record);
            }
            fixture
        }
        fn outcome_path(&self, index: usize) -> PathBuf {
            self.directory
                .join(".asb-completions")
                .join(format!("{index}.json"))
        }
        pub(super) fn read(&self, index: usize) -> Value {
            read_private(&self.outcome_path(index)).unwrap()
        }
        pub(super) fn write(&self, index: usize, record: &Value) {
            std::fs::write(
                self.outcome_path(index),
                serde_json::to_vec(record).unwrap(),
            )
            .unwrap();
        }
        pub(super) fn seal(&self) -> Result<(), Error> {
            self.inputs.completed(0, &self.directory, &self.entry)
        }
        fn verify(&self) -> Result<(Completion, Vec<Value>), Error> {
            self.inputs
                .verify_completed(0, &self.directory, &self.entry)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn historical_measured_legacy_null_execution_can_seal_and_reuse_false_grades() {
        let mut fixture = Fixture::new(false);
        fixture.entry.configuration.measurements = Some(crate::benchmark::MeasurementConfig {
            enabled: true,
            directory: "measurements".into(),
            collectors: Vec::new(),
        });
        for index in 0..2 {
            let mut outcome = fixture.read(index);
            let measured = outcome["summary"].clone();
            outcome["summary"]["execution"] = Value::Null;
            fixture.write(index, &outcome);
            let directory = fixture
                .directory
                .join(format!("measurements-concurrency-{}", index + 1));
            std::fs::create_dir(&directory).unwrap();
            std::fs::write(
                directory.join("run.json"),
                serde_json::to_vec(&measured).unwrap(),
            )
            .unwrap();
        }
        fixture.seal().unwrap();
        let (_, summaries) = fixture.verify().unwrap();
        assert_eq!(summaries.len(), 2);
        assert!(
            summaries
                .iter()
                .all(|summary| summary["workload_success"] == false)
        );
        let receipt = std::fs::read(fixture.inputs.completion_path(0)).unwrap();
        fixture.seal().unwrap();
        assert_eq!(
            std::fs::read(fixture.inputs.completion_path(0)).unwrap(),
            receipt
        );
    }

    #[test]
    fn completed_false_and_true_outcomes_are_sealed_with_private_modes() {
        for resolved in [false, true] {
            let fixture = Fixture::new(resolved);
            fixture.seal().unwrap();
            fixture.verify().unwrap();
            let path = fixture.inputs.completion_path(0);
            let before = std::fs::read(&path).unwrap();
            let text = String::from_utf8(before.clone()).unwrap();
            assert!(!text.contains("private evaluator sentinel"));
            assert!(!text.contains("private transcript sentinel"));
            fixture.seal().unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(
                std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
                1
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
                assert_eq!(
                    std::fs::metadata(path.parent().unwrap())
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o700
                );
            }
        }
    }

    #[test]
    fn wire_compatibility_keeps_legacy_groups_and_rejects_unknown_or_incomplete_records() {
        for case in [
            "legacy",
            "process-false",
            "unknown-field",
            "unknown-task-field",
            "unknown-baseline-field",
            "missing-proxy-cleanup",
            "legacy-grade-only",
            "missing-process",
            "malformed-json",
            "cohort-without-slot",
        ] {
            let fixture = Fixture::new(false);
            let mut record = fixture.read(0);
            match case {
                "legacy" => (),
                "process-false" => record["summary"]["process_success"] = json!(false),
                "unknown-field" => record["unknown"] = json!(true),
                "unknown-task-field" => record["summary"]["task"]["unknown"] = json!(true),
                "unknown-baseline-field" => {
                    record["summary"]["workload_baseline"]["unknown"] = json!(true)
                }
                "missing-proxy-cleanup" => {
                    record
                        .as_object_mut()
                        .unwrap()
                        .remove("proxy_cleanup_confirmed");
                }
                "legacy-grade-only" => {
                    record["summary"]
                        .as_object_mut()
                        .unwrap()
                        .remove("workload_baseline");
                    record["summary"]["workload_baseline_passed"] = json!(false);
                }
                "missing-process" => {
                    record["summary"]
                        .as_object_mut()
                        .unwrap()
                        .remove("process_success");
                }
                "malformed-json" => (),
                "cohort-without-slot" => {
                    record["summary"]["execution_cohort"] =
                        json!({"id":"abc-123","selected_replicas":2,"admitted_slots":[1,2]})
                }
                _ => unreachable!(),
            }
            fixture.write(0, &record);
            if case == "malformed-json" {
                std::fs::write(fixture.outcome_path(0), b"{unfinished").unwrap();
            }
            let valid = matches!(case, "legacy" | "process-false");
            assert_eq!(fixture.seal().is_ok(), valid, "{case}");
            assert_eq!(fixture.inputs.completion_path(0).exists(), valid, "{case}");
            if valid {
                fixture.verify().unwrap();
            }
        }
    }

    #[test]
    fn evidence_or_result_changes_cannot_replace_a_historical_completion() {
        for external in [false, true] {
            let fixture = Fixture::new(false);
            fixture.seal().unwrap();
            let path = fixture.inputs.completion_path(0);
            let original = std::fs::read(&path).unwrap();
            // Presentation spools live outside the per-task evidence tree.
            std::fs::write(
                fixture.root.join(".terminal-output-000000.tmp"),
                "temporary output",
            )
            .unwrap();
            fixture.verify().unwrap();
            let changed = if external {
                fixture.evidence.join("grade.json")
            } else {
                fixture.directory.join("run.log")
            };
            std::fs::write(changed, "changed evidence").unwrap();
            assert!(fixture.verify().is_err());
            assert!(fixture.seal().is_err());
            assert_eq!(std::fs::read(path).unwrap(), original);
        }
    }

    #[test]
    fn external_proxy_outputs_and_their_observed_absence_are_bound() {
        for initially_present in [false, true] {
            let fixture = Fixture::new(false);
            let output = fixture.root.join("external-recording.jsonl");
            if initially_present {
                std::fs::write(&output, "private recording sentinel").unwrap();
            }
            for index in 0..2 {
                let mut record = fixture.read(index);
                record["proxy_evidence"] = json!([output]);
                fixture.write(index, &record);
            }
            fixture.seal().unwrap();
            fixture.verify().unwrap();
            let encoded = std::fs::read_to_string(fixture.inputs.completion_path(0)).unwrap();
            assert!(!encoded.contains("private recording sentinel"));
            std::fs::write(&output, "different recording").unwrap();
            assert!(fixture.verify().is_err());
        }
    }

    #[test]
    fn custom_policy_without_recorded_module_cannot_bypass_selected_protocol() {
        let fixture = Fixture::new(false);
        let mut record = fixture.read(0);
        record["summary"]["module"] = Value::Null;
        record["summary"]["workload_success"] = Value::Null;
        record["summary"]["benchmark_outcome"] = json!({
            "schema_version":1,"execution_success":true,
            "evaluation":{"status":"completed","resolution":null,"rewards":{"accuracy":0.5}},
            "cleanup_confirmed":true,"evidence":[fixture.evidence],
            "policy":{"id":"accuracy","version":1,"reward":"accuracy","aggregation":"attempt-mean",
                "rewards":{"accuracy":{"unit":"ratio","range":[0,1]}}},
            "lineage":{"execution_attempt":record["summary"]["task"]["attempt_id"],"evaluation_id":"eval-fixture","supersedes":null}
        });
        fixture.write(0, &record);
        let error = fixture.seal().unwrap_err();
        assert!(error.to_string().contains("metric policy"), "{error}");
        assert!(!fixture.inputs.completion_path(0).exists());
    }

    #[test]
    fn completed_native_evaluation_without_primary_reward_cannot_be_sealed() {
        for rewards in [json!({}), json!({"secondary":0.5})] {
            let fixture = Fixture::native();
            let mut record = fixture.read(0);
            record["summary"]["benchmark_outcome"]["evaluation"]["rewards"] = rewards;
            fixture.write(0, &record);
            assert!(ValidatedOutcome::read(&fixture.outcome_path(0), &fixture.entry).is_err());
            assert!(fixture.seal().is_err());
            assert!(!fixture.inputs.completion_path(0).exists());
        }
    }

    #[test]
    fn completed_native_rewards_keep_null_resolution_and_both_process_results() {
        for process_success in [false, true] {
            for recorded_module in [false, true] {
                let fixture = Fixture::native();
                for index in 0..2 {
                    let mut record = fixture.read(index);
                    record["summary"]["process_success"] = json!(process_success);
                    record["summary"]["benchmark_outcome"]["execution_success"] =
                        json!(process_success);
                    if !recorded_module {
                        record["summary"].as_object_mut().unwrap().remove("module");
                    }
                    fixture.write(index, &record);
                    let validated =
                        ValidatedOutcome::read(&fixture.outcome_path(index), &fixture.entry)
                            .unwrap();
                    assert!(validated.summary["workload_success"].is_null());
                }
                fixture.seal().unwrap();
                let (_, summaries) = fixture.verify().unwrap();
                assert_eq!(summaries.len(), 2);
                for summary in summaries {
                    assert!(summary["workload_success"].is_null());
                    assert_eq!(summary["process_success"], process_success);
                    assert_eq!(
                        summary["benchmark_outcome"]["evaluation"]["rewards"]["accuracy"],
                        0.5
                    );
                }
            }
        }
    }

    #[test]
    fn structured_binary_outcomes_retain_legacy_schema_one_false_and_true_grades() {
        use crate::workloads::outcome::{EvaluationOutcome, MetricPolicy};
        for resolved in [false, true] {
            let fixture = Fixture::new(resolved);
            for index in 0..2 {
                let mut record = fixture.read(index);
                record["summary"]["benchmark_outcome"] = json!({
                    "schema_version":1,"execution_success":true,
                    "evaluation":EvaluationOutcome::binary(resolved),
                    "cleanup_confirmed":true,"evidence":[fixture.evidence],
                    "policy":MetricPolicy::default(),
                    "lineage":{"execution_attempt":record["summary"]["task"]["attempt_id"],
                        "evaluation_id":format!("binary-fixture-{index}"),"supersedes":null}
                });
                fixture.write(index, &record);
                ValidatedOutcome::read(&fixture.outcome_path(index), &fixture.entry).unwrap();
            }
            fixture.seal().unwrap();
            assert!(
                fixture
                    .verify()
                    .unwrap()
                    .1
                    .iter()
                    .all(|summary| summary["workload_success"] == resolved)
            );
        }
    }

    #[test]
    fn structured_native_invariant_mutations_cannot_publish_completion_receipts() {
        for (name, pointer, replacement) in [
            ("malformed", "/summary/benchmark_outcome", json!({})),
            (
                "schema",
                "/summary/benchmark_outcome/schema_version",
                json!(2),
            ),
            (
                "pending",
                "/summary/benchmark_outcome/evaluation",
                json!({"status":"pending","reason":"queued"}),
            ),
            (
                "error",
                "/summary/benchmark_outcome/evaluation",
                json!({"status":"error","reason":"unavailable"}),
            ),
            (
                "unknown evaluation",
                "/summary/benchmark_outcome/evaluation",
                json!({"status":"unknown"}),
            ),
            (
                "primary missing",
                "/summary/benchmark_outcome/evaluation/rewards",
                json!({"secondary":0.5}),
            ),
            (
                "reward range",
                "/summary/benchmark_outcome/evaluation/rewards/accuracy",
                json!(1.5),
            ),
            (
                "policy id",
                "/summary/benchmark_outcome/policy/id",
                json!("other"),
            ),
            (
                "policy version",
                "/summary/benchmark_outcome/policy/version",
                json!(2),
            ),
            (
                "policy invalid",
                "/summary/benchmark_outcome/policy/aggregation",
                json!("unknown"),
            ),
            ("module malformed", "/summary/module", json!({})),
            ("module invalid", "/summary/module/schema_version", json!(2)),
            (
                "module mismatch",
                "/summary/module/suite_version",
                json!("other"),
            ),
            (
                "module policy",
                "/summary/module/protocol/score_policy",
                json!("metric:other@1"),
            ),
            (
                "lineage attempt",
                "/summary/benchmark_outcome/lineage/execution_attempt",
                json!("other"),
            ),
            (
                "lineage evaluation",
                "/summary/benchmark_outcome/lineage/evaluation_id",
                json!(""),
            ),
            (
                "lineage supersession",
                "/summary/benchmark_outcome/lineage/supersedes",
                json!("eval-fixture-1"),
            ),
            (
                "process mismatch",
                "/summary/benchmark_outcome/execution_success",
                json!(false),
            ),
            (
                "process unknown",
                "/summary/benchmark_outcome/execution_success",
                Value::Null,
            ),
            (
                "resolution mismatch",
                "/summary/workload_success",
                json!(true),
            ),
            (
                "resolution malformed",
                "/summary/workload_success",
                json!("unknown"),
            ),
            (
                "structured resolution",
                "/summary/benchmark_outcome/evaluation/resolution",
                json!(false),
            ),
            (
                "cleanup false",
                "/summary/benchmark_outcome/cleanup_confirmed",
                json!(false),
            ),
            (
                "cleanup unknown",
                "/summary/benchmark_outcome/cleanup_confirmed",
                Value::Null,
            ),
            (
                "evidence absent",
                "/summary/benchmark_outcome/evidence",
                json!([]),
            ),
            (
                "evidence relative",
                "/summary/benchmark_outcome/evidence",
                json!(["relative"]),
            ),
            (
                "evidence mismatch",
                "/summary/benchmark_outcome/evidence",
                Value::Null,
            ),
            (
                "evidence bound",
                "/summary/benchmark_outcome/evidence",
                json!(vec!["/missing"; 65]),
            ),
        ] {
            let fixture = Fixture::native();
            let mut record = fixture.read(1);
            *record.pointer_mut(pointer).unwrap() = if name == "evidence mismatch" {
                json!([fixture.directory])
            } else {
                replacement
            };
            fixture.write(1, &record);
            assert!(
                ValidatedOutcome::read(&fixture.outcome_path(1), &fixture.entry).is_err(),
                "constructor: {name}"
            );
            assert!(fixture.seal().is_err(), "seal: {name}");
            assert!(
                !fixture.inputs.completion_path(0).exists(),
                "receipt: {name}"
            );
        }
    }

    #[test]
    fn incomplete_wrong_or_duplicated_outcomes_never_produce_a_receipt() {
        for case in [
            "cleanup",
            "proxy-cleanup",
            "grade",
            "task",
            "dataset",
            "agent",
            "provider",
            "model",
            "baseline",
            "scope",
            "grader",
            "duplicate",
            "duplicate-replica",
            "missing",
            "extra",
            "temporary",
            "relative-evidence",
            "relative-proxy-output",
            "missing-evidence",
            "no-evidence",
        ] {
            let fixture = Fixture::new(false);
            let mut record = fixture.read(0);
            match case {
                "cleanup" => record["workload_cleanup_confirmed"] = json!(false),
                "proxy-cleanup" => record["proxy_cleanup_confirmed"] = json!(false),
                "grade" => record["summary"]["workload_success"] = Value::Null,
                "task" => record["summary"]["task"]["task_id"] = json!("different-task"),
                "dataset" => {
                    record["summary"]["task"]["dataset"]["revision"] = json!("different-revision")
                }
                "agent" | "provider" | "model" => record["summary"][case] = json!("different"),
                "baseline" => {
                    record["summary"]["workload_baseline"] = json!({"kind":"unavailable"})
                }
                "scope" => record["scope"] = json!("not-an-outcome"),
                "grader" => record["summary"]["workload_evaluation"] = json!("adapter-smoke"),
                "duplicate" => {
                    record["summary"]["task"] = fixture.read(1)["summary"]["task"].clone()
                }
                "duplicate-replica" => {
                    record["schema_version"] = json!(2);
                    record["replica"] = json!({"index":1,"total":2});
                    let mut sibling = fixture.read(1);
                    sibling["schema_version"] = json!(2);
                    sibling["replica"] = record["replica"].clone();
                    fixture.write(1, &sibling);
                }
                "missing" => std::fs::remove_file(fixture.outcome_path(1)).unwrap(),
                "extra" => fixture.write(2, &fixture.read(1)),
                "temporary" => std::fs::write(
                    fixture.directory.join(".asb-completions/.unfinished.tmp"),
                    "partial",
                )
                .unwrap(),
                "relative-evidence" => record["workload_evidence"] = json!(["relative"]),
                "relative-proxy-output" => record["proxy_evidence"] = json!(["relative"]),
                "missing-evidence" => {
                    record["workload_evidence"] = json!([fixture.root.join("missing")])
                }
                "no-evidence" => record["workload_evidence"] = json!([]),
                _ => unreachable!(),
            }
            fixture.write(0, &record);
            assert!(fixture.seal().is_err(), "{case}");
            assert!(!fixture.inputs.completion_path(0).exists(), "{case}");
        }
    }

    #[test]
    fn selection_input_or_receipt_changes_prevent_verification() {
        for case in ["model", "replicas", "input", "digest", "index"] {
            let mut fixture = Fixture::new(true);
            fixture.seal().unwrap();
            match case {
                "model" => fixture.entry.configuration.model = "different".into(),
                "replicas" => fixture.entry.configuration.concurrency = 3,
                "input" => fixture.inputs.initial.digest = "b".repeat(64),
                "digest" | "index" => {
                    let path = fixture.inputs.completion_path(0);
                    let mut receipt: Value = read_private(&path).unwrap();
                    if case == "digest" {
                        receipt["fingerprint"]["digest"] = json!("b".repeat(64));
                    } else {
                        receipt["index"] = json!(1);
                    }
                    std::fs::write(path, serde_json::to_vec(&receipt).unwrap()).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(fixture.verify().is_err(), "{case}");
        }
    }

    #[test]
    fn enabled_measurements_must_match_each_unique_outcome() {
        for case in [
            "matching",
            "missing",
            "grade",
            "duplicate",
            "tokens",
            "module",
            "fingerprint",
            "benchmark-outcome",
        ] {
            let mut fixture = Fixture::new(true);
            fixture.entry.configuration.measurements = Some(crate::benchmark::MeasurementConfig {
                enabled: true,
                directory: "original-setting-is-isolated".into(),
                collectors: Vec::new(),
            });
            for replica in 1..=2 {
                let directory = fixture
                    .directory
                    .join(format!("measurements-concurrency-{replica}"));
                std::fs::create_dir(&directory).unwrap();
                let mut summary = fixture.read(replica - 1)["summary"].clone();
                if replica == 2 {
                    match case {
                        "missing" => continue,
                        "grade" => summary["workload_success"] = json!(false),
                        "duplicate" => summary = fixture.read(0)["summary"].clone(),
                        "tokens" => summary["token_usage"] = json!({"total_tokens":999}),
                        "module" => summary["module"] = json!({"id":"different"}),
                        "fingerprint" => summary["execution_plan_fingerprint"] = json!("different"),
                        "benchmark-outcome" => {
                            summary["benchmark_outcome"] = json!({"schema_version":1})
                        }
                        _ => (),
                    }
                }
                std::fs::write(
                    directory.join("run.json"),
                    serde_json::to_vec(&summary).unwrap(),
                )
                .unwrap();
            }
            assert_eq!(fixture.seal().is_ok(), case == "matching", "{case}");
            if case == "matching" {
                fixture.verify().unwrap();
            } else {
                assert!(!fixture.inputs.completion_path(0).exists());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_or_oversized_outcome_records_cannot_be_sealed() {
        for link in [false, true] {
            let fixture = Fixture::new(true);
            let path = fixture.outcome_path(0);
            if link {
                let other = fixture.root.join("other.json");
                std::fs::rename(&path, &other).unwrap();
                std::os::unix::fs::symlink(other, path).unwrap();
            } else {
                std::fs::write(path, vec![b' '; LIMIT as usize + 1]).unwrap();
            }
            assert!(fixture.seal().is_err());
            assert!(!fixture.inputs.completion_path(0).exists());
        }
    }
}
