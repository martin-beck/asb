//! Report actual outcomes independently of optional runtime measurements.
//!
//! These are recorded results, not verification of a resume seal or historical
//! attestation. Cleanup failures do not erase a separately confirmed task grade.
use super::*;
use crate::runner::replica::Identity;
use std::io::Read;

const LIMIT: u64 = 1024 * 1024;

/// Preserve recorded policy metadata, never reconstruct it from today's
/// registry, configuration or host defaults. Measurements can omit this
/// metadata when their matching outcome/log retains the original object.
pub(super) fn recorded_object(
    measured: &Value,
    outcome: Option<&Recorded>,
    field: &str,
) -> Result<Option<Value>, Error> {
    crate::planning::recorded_object(measured, outcome.map(|record| &record.summary), field)
        .map(|value| value.cloned())
}

/// Called only after current-slot identity and measurement agreement checks.
/// An omitted optional observation must not hide its matching retained fact.
pub(super) fn recorded_observation(
    measured: &Value,
    outcome: Option<&Recorded>,
) -> Result<crate::process_termination::Observation, Error> {
    use crate::process_termination::{Observation, recorded};
    let primary = recorded(measured).map_err(|error| Error::Input(error.into()))?;
    let retained = outcome
        .map(|record| recorded(&record.summary))
        .transpose()
        .map_err(|error| Error::Input(error.into()))?
        .flatten();
    if primary.zip(retained).is_some_and(|(a, b)| a != b) {
        return Err(Error::Input(
            "current-attempt termination facts disagree".into(),
        ));
    }
    if let Some(termination) = primary.or(retained) {
        return Ok(Observation {
            process_termination: Some(termination),
            ..Default::default()
        });
    }
    let mut observation = Observation::from_record(measured);
    if observation.process_termination_diagnostic.is_none() {
        observation.process_termination_diagnostic = outcome.and_then(|record| {
            Observation::from_record(&record.summary).process_termination_diagnostic
        });
    }
    Ok(observation)
}

pub(crate) struct Recorded {
    pub path: PathBuf,
    pub summary: Value,
    workload_cleanup: bool,
    proxy_cleanup: bool,
    pub source: &'static str,
}

impl Recorded {
    pub fn check_measurement(&self, measured: &Value) -> Result<(), Error> {
        crate::process_termination::agree(&self.summary, measured)
            .map_err(|error| invalid(&self.path, error))?;
        if matches!(
            self.source,
            "imported-official-grade" | "recovered-official-grade"
        ) {
            return Err(invalid(
                &self.path,
                "a grade-only result and a measurement claim the same slot; preserve both and inspect the conflict",
            ));
        }
        for field in [
            "execution",
            "recording",
            "native_replay_consumed",
            "replay_selection",
            "execution_plan_fingerprint",
            "module",
            "benchmark_outcome",
            "schema_version",
            "replica_index",
            "execution_cohort",
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
        ] {
            let agrees = if matches!(
                field,
                "workload_resource_disposition" | "workload_artifacts"
            ) {
                self.summary.get(field) == measured.get(field)
            } else {
                self.summary[field] == measured[field]
            };
            if !agrees {
                return Err(invalid(
                    &self.path,
                    &format!(
                        "task outcome and measurement disagree on `{field}` ({}); preserve both records and inspect the attempt",
                        field.replace('_', " ")
                    ),
                ));
            }
        }
        if self.summary["approximate_replay"]
            .as_bool()
            .unwrap_or(false)
            != measured["approximate_replay"].as_bool().unwrap_or(false)
        {
            return Err(invalid(
                &self.path,
                "task outcome and measurement disagree on approximate replay status",
            ));
        }
        Ok(())
    }

    pub fn status(&self) -> String {
        if self.source == "recovered-official-grade" {
            return format!(
                "recovered official task {}; execution timing, tokens, steps and process status unknown; partial execution artifacts preserved, not used as final measurements",
                if self.summary["workload_success"] == true {
                    "resolved"
                } else {
                    "unresolved"
                }
            );
        }
        if self.source == "imported-official-grade" {
            return format!(
                "imported official task {}; execution timing, tokens, steps and process status unknown; recorded cleanup: {}; not execution recovery or resume evidence",
                if self.summary["workload_success"] == true {
                    "resolved"
                } else {
                    "unresolved"
                },
                self.summary["recorded_cleanup"]
                    .as_str()
                    .unwrap_or("unknown")
            );
        }
        let kind = crate::workloads::scored_kind(
            self.summary["workload_evaluation"]
                .as_str()
                .unwrap_or("unknown"),
            &crate::workloads::BaselineEvidence::from_record(&self.summary),
        );
        let grade = if crate::workloads::SavedEvaluationKind::classify(
            self.summary["workload_evaluation"]
                .as_str()
                .unwrap_or("unknown"),
        ) == crate::workloads::SavedEvaluationKind::Known(
            crate::workloads::KnownEvaluationKind::AdapterSmoke,
        ) {
            "adapter smoke only; upstream task resolution not measured"
        } else if !crate::workloads::resolution_eligible(kind) {
            "recorded task unscored; grading scope or baseline is not eligible"
        } else if self.summary["benchmark_outcome"]["evaluation"]["status"] == "completed"
            && self.summary["benchmark_outcome"]["evaluation"]["resolution"].is_null()
        {
            "recorded native rewards evaluated; resolution not applicable"
        } else {
            match self.summary["workload_success"].as_bool() {
                Some(true) => "recorded task resolved",
                Some(false) => "recorded task unresolved",
                None => "recorded task evaluation unavailable",
            }
        };
        let source = if self.source == "run-log" {
            "latest run log"
        } else {
            "task outcome"
        };
        let mut status = format!("{grade}; optional measurements unavailable (using {source})");
        if let Some(reason) = self.summary["failure_reason"].as_str() {
            status.push_str(&format!("; run failure: {reason}"));
        } else if self.summary["process_success"] == false {
            status.push_str("; agent process failed");
        }
        if let Some(reason) = self.summary["workload_failure"]["reason"].as_str() {
            status.push_str(&format!("; evaluator: {reason}"));
        }
        if !self.workload_cleanup || !self.proxy_cleanup {
            status.push_str("; owned-resource cleanup not confirmed");
        }
        if self.summary["workload_resource_disposition"] == "stopped-preserved" {
            status.push_str("; workload resources stopped and preserved, not removed");
        }
        status
    }
}

pub(crate) fn load(directory: &Path, total: usize) -> Result<BTreeMap<usize, Recorded>, Error> {
    let directory = directory.join(".asb-completions");
    match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => {
            return Err(invalid(
                &directory,
                &format!("cannot inspect outcomes: {error}"),
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(invalid(&directory, "outcomes must be a real directory"));
        }
        Ok(_) => (),
    }
    let mut outcomes = BTreeMap::new();
    let mut files = Vec::new();
    for entry in fs::read_dir(&directory).map_err(crate::io_error)? {
        if files.len() >= 2 * crate::benchmark::MAX_CONCURRENCY {
            return Err(invalid(
                &directory,
                "outcome file population exceeds its limit",
            ));
        }
        files.push(entry.map_err(crate::io_error)?.path());
    }
    // Deterministic validation diagnostics, never a replica mapping. Actual
    // slots still come from explicit identity or a unique measured attempt.
    files.sort();
    for path in files {
        // A stopped writer can leave an unpublished temporary record. It earns
        // no grade, but must not hide the completed results of other replicas.
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.') && name.ends_with(".tmp"))
        {
            continue;
        }
        if path.extension().is_none_or(|extension| extension != "json") {
            return Err(invalid(&path, "unexpected file in outcome directory"));
        }
        let raw = bounded_record(&path)?;
        let (replica, recorded) = decode_outcome(&path, &raw, total)?;
        if outcomes.insert(replica, recorded).is_some() {
            return Err(invalid(
                &path,
                "multiple task outcomes claim the same replica slot",
            ));
        }
    }
    Ok(outcomes)
}

/// Structural report decoding, deliberately weaker than resume eligibility.
pub(super) fn decode_outcome(
    path: &Path,
    raw: &Value,
    total: usize,
) -> Result<(usize, Recorded), Error> {
    let summary = &raw["summary"];
    // Original schema-1 records predate proxy cleanup evidence. Absence is
    // unknown cleanup, not a fabricated confirmation or an erased grade.
    let proxy_cleanup = match raw.get("proxy_cleanup_confirmed") {
        None if raw["schema_version"] == 1 => Some(false),
        value => value.and_then(Value::as_bool),
    };
    let replica = if raw["schema_version"] == 2 {
        serde_json::from_value::<Identity>(raw["replica"].clone())
            .map_err(|_| invalid(path, "outcome replica identity is malformed"))?
    } else if raw["schema_version"] == 1 {
        legacy_replica(
            path.parent()
                .and_then(Path::parent)
                .ok_or_else(|| invalid(path, "outcome has no parent directory"))?,
            path,
            summary,
            total,
        )?
    } else {
        return Err(invalid(
            path,
            "outcome schema or legacy replica mapping is unsupported",
        ));
    };
    if !replica.valid()
        || replica.total != total
        || raw["scope"] != "task-outcome-not-yet-a-resume-receipt"
        || raw["workload_cleanup_confirmed"].as_bool().is_none()
        || proxy_cleanup.is_none()
        || !summary.is_object()
        || summary["schema_version"] != 2
        || summary["process_success"].as_bool().is_none()
        || (!summary["workload_success"].is_null()
            && summary["workload_success"].as_bool().is_none())
        || !summary["task"].is_object()
        || summary["replica_index"]
            .as_u64()
            .is_some_and(|index| index != replica.index as u64)
    {
        return Err(invalid(
            path,
            "outcome identity or result fields are malformed",
        ));
    }
    if !crate::workloads::ResourceDisposition::matches_record(
        raw.get("workload_resource_disposition"),
        raw["workload_cleanup_confirmed"] == true,
        summary,
    ) {
        return Err(invalid(
            path,
            if raw["workload_cleanup_confirmed"] == false {
                "outcome workload or proxy cleanup is unconfirmed; resource disposition contradicts cleanup"
            } else {
                "outcome resource disposition contradicts cleanup confirmation"
            },
        ));
    }
    let mut summary = summary.clone();
    if let Some(object) = summary.as_object_mut() {
        object.remove("stdout");
        object.remove("stderr");
    }
    Ok((
        replica.index,
        Recorded {
            path: path.to_path_buf(),
            summary,
            workload_cleanup: raw["workload_cleanup_confirmed"] == true,
            proxy_cleanup: proxy_cleanup == Some(true),
            source: "task-outcome",
        },
    ))
}

/// Ordinary runs append logs and may share an outcome directory with other
/// benchmarks. Select the last complete record of each configured replica log,
/// never arbitrary outcome filenames or another benchmark's measurements.
pub(crate) fn load_run(
    config_path: &Path,
    config: &crate::benchmark::BenchmarkConfig,
) -> Result<BTreeMap<usize, Recorded>, Error> {
    load_logs(config_path, config, |_| true)
}

fn load_logs(
    config_path: &Path,
    config: &crate::benchmark::BenchmarkConfig,
    include: impl Fn(usize) -> bool,
) -> Result<BTreeMap<usize, Recorded>, Error> {
    use std::io::{Seek, SeekFrom};
    let mut records = BTreeMap::new();
    for index in 1..=config.concurrency {
        if !include(index) {
            continue;
        }
        let selected = crate::runner::replica::configuration(
            config,
            Identity {
                index,
                total: config.concurrency,
            },
        )?;
        let path = crate::runner::resolve_log_path(config_path, &selected.log_file);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(invalid(&path, &format!("cannot inspect run log: {error}"))),
            Ok(metadata) if !metadata.is_file() => {
                return Err(invalid(
                    &path,
                    "run log must be a regular file, not a symlink or pipe",
                ));
            }
            Ok(_) => (),
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = match options.open(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            result => result.map_err(|error| invalid(&path, &error.to_string()))?,
        };
        let metadata = file.metadata().map_err(crate::io_error)?;
        if !metadata.is_file() {
            return Err(invalid(&path, "run log must be a regular file"));
        }
        let size = metadata.len();
        let offset = size.saturating_sub(super::MAX_BYTES);
        file.seek(SeekFrom::Start(offset))
            .map_err(crate::io_error)?;
        let mut tail = Vec::new();
        file.take(super::MAX_BYTES + 1)
            .read_to_end(&mut tail)
            .map_err(crate::io_error)?;
        if tail.len() as u64 > super::MAX_BYTES {
            return Err(invalid(
                &path,
                "log changed while reading; retry after the run finishes",
            ));
        }
        let line = tail
            .split(|byte| *byte == b'\n')
            .rfind(|line| !line.iter().all(u8::is_ascii_whitespace))
            .ok_or_else(|| invalid(&path, "run log is empty"))?;
        if offset > 0 && line.len() == tail.len() {
            return Err(invalid(&path, "latest run log record exceeds 16 MiB"));
        }
        let summary: Value = serde_json::from_slice(line)
            .map_err(|error| invalid(&path, &format!("invalid latest run log JSON: {error}")))?;
        records.insert(index, decode_log(&path, summary)?);
    }
    Ok(records)
}

pub(super) fn decode_log(path: &Path, mut summary: Value) -> Result<Recorded, Error> {
    if summary["schema_version"] != 2
        || !summary["task"].is_object()
        || summary["process_success"].as_bool().is_none()
        || (!summary["workload_success"].is_null()
            && summary["workload_success"].as_bool().is_none())
    {
        return Err(invalid(
            path,
            "latest run log lacks a supported task summary",
        ));
    }
    // Logs include client transcripts; reports must never copy them.
    if let Some(object) = summary.as_object_mut() {
        object.remove("stdout");
        object.remove("stderr");
    }
    Ok(Recorded {
        path: path.to_path_buf(),
        summary,
        workload_cleanup: false,
        proxy_cleanup: false,
        source: "run-log",
    })
}

/// Sweep execution owns these fixed isolated log paths. Logs supply reporting
/// evidence only, never completion records or resume/sealing permission. Compare
/// subsequently validates selected configuration/task/policy/cohort bindings.
pub(crate) fn add_sweep_logs(
    records: &mut BTreeMap<usize, Recorded>,
    directory: &Path,
    configuration: &crate::benchmark::BenchmarkConfig,
) -> Result<(), Error> {
    let mut isolated = configuration.clone();
    // Ignore the source benchmark's user-controlled log location: the actual
    // sweep executor rewrites it into this isolated result directory.
    isolated.log_file = std::path::absolute(directory.join("run.log"))
        .map_err(crate::io_error)?
        .to_string_lossy()
        .into_owned();
    // Explicit retained-grade imports/closures intentionally leave execution
    // observations unknown. Their partial logs stay diagnostic, not competing
    // execution evidence, and may be incomplete or malformed after interruption.
    let logs = load_logs(&directory.join("benchmark.toml"), &isolated, |slot| {
        !records.get(&slot).is_some_and(|record| {
            matches!(
                record.source,
                "imported-official-grade" | "recovered-official-grade"
            )
        })
    })?;
    for (slot, log) in logs {
        let declared = log.summary["replica_index"].as_u64();
        if declared.is_some_and(|index| index != slot as u64)
            || (configuration.concurrency > 1 && declared != Some(slot as u64))
        {
            return Err(invalid(
                &log.path,
                "latest sweep log does not match its replica slot",
            ));
        }
        if let Some(recorded) = records.get_mut(&slot) {
            // A newer/different log cannot silently borrow an older outcome's
            // grade or cleanup. Reject conflicting observations before publish.
            recorded.check_measurement(&log.summary)?;
            for field in ["planning", "effective_budgets"] {
                if let Some(value) = recorded_object(&log.summary, Some(recorded), field)? {
                    recorded.summary[field] = value;
                }
            }
        } else {
            records.insert(slot, log);
        }
    }
    Ok(())
}

pub(super) fn add_imports(
    outcomes: &mut BTreeMap<usize, Recorded>,
    imports: BTreeMap<usize, (PathBuf, Value)>,
) -> Result<(), Error> {
    add_retained(outcomes, imports, "imported-official-grade")
}

pub(super) fn add_closed(
    outcomes: &mut BTreeMap<usize, Recorded>,
    closed: BTreeMap<usize, (PathBuf, Value)>,
) -> Result<(), Error> {
    add_retained(outcomes, closed, "recovered-official-grade")
}

fn add_retained(
    outcomes: &mut BTreeMap<usize, Recorded>,
    imports: BTreeMap<usize, (PathBuf, Value)>,
    source: &'static str,
) -> Result<(), Error> {
    for (slot, (path, summary)) in imports {
        if outcomes.contains_key(&slot) {
            return Err(invalid(
                &path,
                "import conflicts with an existing task outcome; preserve both",
            ));
        }
        outcomes.insert(
            slot,
            Recorded {
                path,
                summary,
                workload_cleanup: false,
                proxy_cleanup: false,
                source,
            },
        );
    }
    Ok(())
}

fn legacy_replica(
    directory: &Path,
    path: &Path,
    summary: &Value,
    total: usize,
) -> Result<Identity, Error> {
    if total == 1 {
        return Ok(Identity::default());
    }
    if let Some(index) = summary["replica_index"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
    {
        return Ok(Identity { index, total });
    }
    // Old multi-replica outcomes did not declare their slot. Retain measured
    // history only when the actual attempt maps to exactly one measured slot;
    // directory iteration order or filenames are never evidence of a slot.
    let mut matching = None;
    for index in 1..=total {
        let measured = directory.join(format!("measurements-concurrency-{index}/run.json"));
        match fs::symlink_metadata(&measured) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(invalid(
                    &measured,
                    &format!("cannot inspect legacy measurement: {error}"),
                ));
            }
            Ok(_) => (),
        }
        let record = bounded_record(&measured)?;
        if summary["task"].is_object()
            && record["task"] == summary["task"]
            && matching.replace(index).is_some()
        {
            return Err(invalid(
                path,
                "legacy attempt appears in multiple measured replica slots",
            ));
        }
    }
    matching.map(|index|Identity {index,total}).ok_or_else(||invalid(path,
        "legacy outcome has no unambiguous replica mapping; preserve it and its original measurements"))
}

fn bounded_record(path: &Path) -> Result<Value, Error> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|error| invalid(path, &format!("cannot read outcome: {error}")))?;
    let metadata = file.metadata().map_err(crate::io_error)?;
    if !metadata.is_file() || metadata.len() > LIMIT {
        return Err(invalid(path, "outcome must be a bounded regular file"));
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(crate::io_error)?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid(path, "outcome exceeded its size limit"));
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid(path, "outcome JSON is malformed"))
}

fn invalid(path: &Path, reason: &str) -> Error {
    Error::Input(format!(
        "cannot compare task outcome {}: {reason}",
        path.display()
    ))
}

#[cfg(test)]
#[path = "../../tests/unit/outcome_logs.rs"]
mod log_boundary_tests;

#[cfg(test)]
mod tests {
    #[test]
    fn known_termination_conflicts_reject_but_legacy_carriers_and_unknown_do_not() {
        let mut record = super::Recorded {
            path: "outcome.json".into(),
            summary: serde_json::json!({"exit_code":1,"process_termination":{"kind":"signal","signal":15}}),
            workload_cleanup: true,
            proxy_cleanup: true,
            source: "task-outcome",
        };
        assert!(
            record
                .check_measurement(&serde_json::json!({"exit_code":143}))
                .is_ok()
        );
        assert!(record.check_measurement(&serde_json::json!({"exit_code":143,"process_termination":{"kind":"signal","signal":15}})).is_ok());
        assert!(
            record
                .check_measurement(
                    &serde_json::json!({"process_termination":{"kind":"code","code":143}})
                )
                .is_err()
        );
        assert!(
            record
                .check_measurement(
                    &serde_json::json!({"process_termination":{"kind":"signal","signal":0}})
                )
                .is_err()
        );
        record.summary = serde_json::json!({});
        assert!(
            record
                .check_measurement(&serde_json::json!({"process_termination":null}))
                .is_ok()
        );
    }
    #[test]
    fn measurements_preserve_settlement_and_artifact_agreement() {
        let mut record = super::Recorded {
            path: std::path::PathBuf::from("outcome.json"),
            summary: serde_json::json!({}),
            workload_cleanup: true,
            proxy_cleanup: true,
            source: "task-outcome",
        };
        assert!(record.check_measurement(&serde_json::json!({})).is_ok());
        record.summary = serde_json::json!({
            "workload_resource_disposition":"stopped-preserved",
            "workload_artifacts":["/owned/attempt"]});
        assert!(record.check_measurement(&record.summary).is_ok());
        for field in ["workload_resource_disposition", "workload_artifacts"] {
            let mut measured = record.summary.clone();
            measured.as_object_mut().unwrap().remove(field);
            assert!(record.check_measurement(&measured).is_err());
            measured[field] = serde_json::Value::Null;
            assert!(record.check_measurement(&measured).is_err());
        }
        let mut measured = record.summary.clone();
        measured["workload_resource_disposition"] = serde_json::json!("removed");
        assert!(record.check_measurement(&measured).is_err());
    }
    use super::*;

    struct Fixture {
        root: PathBuf,
        total: usize,
        workload: String,
        dataset: Value,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    impl Fixture {
        fn new(workload: &str, total: usize) -> Self {
            let id = crate::workloads::TaskIdentity::local("compare-outcome-test", None).attempt_id;
            let root = std::env::temp_dir().join(format!("asb-outcome-{id}"));
            fs::create_dir_all(root.join("sweep-results/000-one/.asb-completions")).unwrap();
            let text = format!(
                "agent='pi'\nprovider='fixture-provider'\nmodel='fixture-model'\nworkload='{workload}'\ntrial='original-task'\nconcurrency={total}\n"
            );
            fs::write(root.join("one.toml"), &text).unwrap();
            fs::write(root.join("sweep.toml"), "configs=['one.toml']\n").unwrap();
            let config: crate::benchmark::BenchmarkConfig = toml::from_str(&text).unwrap();
            let dataset = json!({"source":"original-fixture-dataset","revision":"a".repeat(40),"split":"test"});
            fs::write(
                root.join("sweep-results/plan.json"),
                json!({
                    "schema_version":1,"run_id":"a123","execution_mode":"replay",
                    "entries":[{"reference":"one.toml","configuration":config,
                    "selection":{"workload":workload,"task_id":"original-task","dataset":dataset,
                    "evaluation_kind":"upstream-harness","population_size":1}}]
                })
                .to_string(),
            )
            .unwrap();
            Self {
                root,
                total,
                workload: workload.into(),
                dataset,
            }
        }

        fn directory(&self) -> PathBuf {
            self.root.join("sweep-results/000-one")
        }

        fn record(&self, slot: usize, grade: Value, process: bool, cleanup: bool) -> Value {
            let baseline = if self.workload.starts_with("terminal-bench") {
                json!({"kind":"upstream-environment-definition","source_revision":"a".repeat(40),"definition_sha256":"b".repeat(64)})
            } else {
                json!({"kind":"upstream-task-definition","base_commit":"a".repeat(40)})
            };
            let summary = json!({
                "schema_version":2,"agent":"pi","provider":"fixture-provider","model":"fixture-model",
                "workload":self.workload,"replica_index":slot,
                "execution_cohort":{"id":"ab123","selected_replicas":self.total,"admitted_slots":(1..=self.total).collect::<Vec<_>>()},
                "task":{"workload":self.workload,"task_id":"original-task","dataset":self.dataset,"attempt_id":format!("ab123-{slot}")},
                "process_success":process,"workload_success":grade,
                "workload_evaluation":"upstream-harness",
                "workload_baseline":baseline,
                "workload_baseline_passed":null,
                "workload_failure":if grade.is_null(){json!({"kind":"evaluation-error","reason":"fixture grader timed out"})}else{Value::Null},
                "failure_kind":if process {Value::Null}else{json!("agent-exit")},
                "failure_reason":if process {Value::Null}else{json!("fixture agent exited nonzero")},
                "success_rate":if grade.is_null(){Value::Null}else{json!(if grade==true {1.0}else{0.0})},
                "elapsed_ms":57,"steps":3,"token_usage":{"total_tokens":23},"adapter_smoke_success":null
            });
            json!({"schema_version":2,"scope":"task-outcome-not-yet-a-resume-receipt",
                "replica":{"index":slot,"total":self.total},
                "workload_cleanup_confirmed":cleanup,"proxy_cleanup_confirmed":cleanup,
                "summary":summary,"workload_evidence":[],"proxy_evidence":[],"outputs":[]})
        }

        fn publish(&self, slot: usize, record: &Value) -> PathBuf {
            let path = self
                .directory()
                .join(format!(".asb-completions/{slot:03}.json"));
            fs::write(&path, record.to_string()).unwrap();
            path
        }

        fn compare(&self) -> Result<Value, Error> {
            super::super::run(
                self.root.join("sweep.toml").to_str().unwrap(),
                &mut Vec::new(),
            )?;
            read_json(&self.root.join("results/sweep-comparison/comparison.json"))
        }
    }

    #[test]
    fn pi_native_consumption_evidence_is_reported_without_inventing_coverage() {
        let fixture = Fixture::new("swe-bench", 2);
        let mut observed = fixture.record(1, json!(true), true, true);
        observed["summary"]["native_replay_consumed"] = json!(true);
        fixture.publish(1, &observed);
        fixture.publish(2, &fixture.record(2, json!(true), true, true));
        let measured_dir = fixture.directory().join("measurements-concurrency-1");
        fs::create_dir(&measured_dir).unwrap();
        fs::write(
            measured_dir.join("run.json"),
            observed["summary"].to_string(),
        )
        .unwrap();
        let report = fixture.compare().unwrap();
        let rows = report["benchmarks"].as_array().unwrap();
        assert_eq!(rows[0]["native_replay_consumed"], true);
        assert!(rows[1]["native_replay_consumed"].is_null());
        let html =
            fs::read_to_string(fixture.root.join("results/sweep-comparison/report.html")).unwrap();
        assert_eq!(
            html.matches("agent adapter consumed its recorded native tool stream")
                .count(),
            1
        );
        assert!(html.contains("does not establish coverage of all processes"));
        let mut different = observed["summary"].clone();
        different["native_replay_consumed"] = json!(false);
        fs::write(measured_dir.join("run.json"), different.to_string()).unwrap();
        assert!(fixture.compare().is_err());
    }

    #[test]
    fn report_discloses_replay_selection_without_asserting_coverage() {
        let fixture = Fixture::new("swe-bench", 4);
        let selections = [
            json!({"provider_replay":true,"tools":"not-selected","mcp_replay":false}),
            json!({"provider_replay":true,"tools":"pi-adapter-local","mcp_replay":false}),
            json!({"provider_replay":true,"tools":"native-snapshot","mcp_replay":false}),
        ];
        for slot in 1..=4 {
            let mut record = fixture.record(slot, json!(true), true, true);
            if slot < 4 {
                record["summary"]["replay_selection"] = selections[slot - 1].clone();
            }
            fixture.publish(slot, &record);
        }
        let report = fixture.compare().unwrap();
        for (row, selection) in report["benchmarks"]
            .as_array()
            .unwrap()
            .iter()
            .take(3)
            .zip(&selections)
        {
            assert_eq!(&row["replay_selection"], selection);
        }
        assert!(report["benchmarks"][3]["replay_selection"].is_null());
        let html =
            fs::read_to_string(fixture.root.join("results/sweep-comparison/report.html")).unwrap();
        assert!(html.contains(
            "tool replay not selected; native commands and external requests may be live"
        ));
        assert!(html.contains("Pi adapter-local tool replay selected"));
        assert!(html.contains("experimental native snapshot tool replay selected"));
        assert!(html.contains("Selection is not proof of complete tool or filesystem coverage"));
        let mut contradictory = fixture.record(1, json!(true), true, true);
        contradictory["summary"]["replay_selection"] = selections[0].clone();
        contradictory["summary"]["native_replay_consumed"] = json!(true);
        fixture.publish(1, &contradictory);
        assert!(fixture.compare().is_err());
    }

    #[test]
    fn non_pi_native_consumption_survives_comparison() {
        let fixture = Fixture::new("swe-bench", 1);
        let config_path = fixture.root.join("one.toml");
        let text = fs::read_to_string(&config_path)
            .unwrap()
            .replace("agent='pi'", "agent='opencode'");
        fs::write(&config_path, text).unwrap();
        let plan_path = fixture.root.join("sweep-results/plan.json");
        let mut plan = read_json(&plan_path).unwrap();
        plan["entries"][0]["configuration"]["agent"] = json!("opencode");
        fs::write(plan_path, plan.to_string()).unwrap();
        let mut record = fixture.record(1, json!(true), true, true);
        record["summary"]["agent"] = json!("opencode");
        record["summary"]["native_replay_consumed"] = json!(true);
        record["summary"]["replay_selection"] =
            json!({"provider_replay":true,"tools":"native-snapshot","mcp_replay":false});
        fixture.publish(1, &record);
        let report = fixture.compare().unwrap();
        assert_eq!(report["benchmarks"][0]["native_replay_consumed"], true);
        assert_eq!(
            report["benchmarks"][0]["replay_selection"]["tools"],
            "native-snapshot"
        );
    }

    #[test]
    fn conflicting_native_consumption_evidence_is_rejected() {
        let record = super::Recorded {
            path: PathBuf::from("outcome.json"),
            summary: json!({"native_replay_consumed":true}),
            workload_cleanup: true,
            proxy_cleanup: true,
            source: "task-outcome",
        };
        assert!(
            record
                .check_measurement(&json!({"native_replay_consumed":true}))
                .is_ok()
        );
        assert!(record.check_measurement(&json!({})).is_err());
        assert!(
            record
                .check_measurement(&json!({"native_replay_consumed":false}))
                .is_err()
        );
    }

    #[test]
    fn policy_metadata_is_not_observed_metrics_with_or_without_measurements() {
        use crate::workloads::outcome::*;
        for style in ["unmeasured", "measured", "measured-with-policy", "unknown"] {
            let fixture = Fixture::new("terminal-bench-2.1", 1);
            let mut record = fixture.record(1, json!(true), true, true);
            let mut module = crate::workloads::descriptor("terminal-bench-2.1").unwrap();
            module.dataset = Some(serde_json::from_value(fixture.dataset.clone()).unwrap());
            record["summary"]["module"] = json!(module);
            record["summary"]["benchmark_outcome"] = json!(BenchmarkOutcome {
                schema_version: 1,
                execution_success: Some(true),
                evaluation: EvaluationOutcome::binary(true),
                cleanup_confirmed: Some(true),
                evidence: vec![],
                policy: MetricPolicy::default(),
                lineage: EvaluationLineage {
                    execution_attempt: record["summary"]["task"]["attempt_id"]
                        .as_str()
                        .unwrap()
                        .into(),
                    evaluation_id: "fixture-evaluation".into(),
                    supersedes: None,
                },
            });
            let planning =
                json!({"reservation":{"memory_mib":8192},"repetition":{"index":1,"total":2}});
            let caps = json!({"execution_seconds":900.0});
            if style != "unknown" {
                record["summary"]["planning"] = planning.clone();
                record["summary"]["effective_budgets"] = caps.clone();
            }
            fixture.publish(1, &record);
            if style != "unmeasured" {
                let directory = fixture.directory().join("measurements");
                fs::create_dir(&directory).unwrap();
                let mut measured = record["summary"].clone();
                if style != "measured-with-policy" {
                    measured.as_object_mut().unwrap().remove("planning");
                    measured
                        .as_object_mut()
                        .unwrap()
                        .remove("effective_budgets");
                }
                fs::write(directory.join("run.json"), measured.to_string()).unwrap();
            }
            let report = fixture.compare().unwrap();
            let row = &report["benchmarks"][0];
            assert_eq!(
                row["planning"],
                if style == "unknown" {
                    Value::Null
                } else {
                    planning
                }
            );
            assert_eq!(
                row["effective_budgets"],
                if style == "unknown" {
                    Value::Null
                } else {
                    caps
                }
            );
            assert_eq!(row["module"]["protocol"]["revision"], 1);
            let metrics = row["metrics"].as_object().unwrap();
            assert_eq!(metrics["elapsed_ms"].as_f64(), Some(57.0));
            assert!(
                metrics
                    .iter()
                    .any(|(key, value)| key.starts_with("benchmark_rewards.")
                        && value.as_f64() == Some(1.0))
            );
            for prefix in [
                "planning.",
                "effective_budgets.",
                "module.",
                "benchmark_outcome.",
            ] {
                assert!(
                    !metrics.keys().any(|key| key.starts_with(prefix)),
                    "{style}: {prefix}"
                );
            }
            for name in [
                "planning.reservation.memory_mib",
                "planning.repetition.index",
                "effective_budgets.execution_seconds",
                "module.protocol.revision",
            ] {
                let filename = GraphNames::default().allocate("metric", name);
                assert!(
                    !fixture
                        .root
                        .join("results/sweep-comparison")
                        .join(filename)
                        .exists()
                );
            }
        }
    }

    #[test]
    fn policy_metadata_conflicts_and_non_objects_fail_without_inventing_defaults() {
        let record = Recorded {
            path: "fixture".into(),
            summary: json!({"planning":{"backend":"native"}}),
            workload_cleanup: true,
            proxy_cleanup: true,
            source: "fixture",
        };
        assert!(
            recorded_object(
                &json!({"planning":{"backend":"other"}}),
                Some(&record),
                "planning"
            )
            .unwrap_err()
            .to_string()
            .contains("disagree")
        );
        assert!(
            recorded_object(&json!({"effective_budgets":3}), None, "effective_budgets").is_err()
        );
        assert!(
            recorded_object(&json!({}), None, "planning")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn execution_plan_fingerprints_partition_known_and_unknown_experiments() {
        let fixture = Fixture::new("terminal-bench-2.1", 3);
        for slot in 1..=3 {
            let mut record = fixture.record(slot, json!(true), true, true);
            if slot < 3 {
                record["summary"]["execution_plan_fingerprint"] = json!(format!("plan-{slot}"));
            }
            fixture.publish(slot, &record);
        }
        let report = fixture.compare().unwrap();
        assert_eq!(report["metric_groups"].as_object().unwrap().len(), 3);
        assert_eq!(report["population_coverage"].as_object().unwrap().len(), 3);
        assert_eq!(report["resolution_groups"].as_object().unwrap().len(), 3);
        assert!(report["benchmarks"][2]["execution_plan_fingerprint"].is_null());
    }

    #[test]
    fn validated_protocol_cohorts_aggregate_upstream_task_defaults_and_separate_operator_overrides()
    {
        let fixture = Fixture::new("terminal-bench-2.1", 1);
        let mut fingerprints = Vec::new();
        let mut entries = Vec::new();
        fs::write(
            fixture.root.join("sweep.toml"),
            "configs=['one.toml','two.toml','three.toml']",
        )
        .unwrap();
        for (index, name) in ["one", "two", "three"].into_iter().enumerate() {
            let slot = index + 1;
            let task = format!("task-{slot}");
            let mut config: crate::benchmark::BenchmarkConfig = toml::from_str(&format!("agent='pi'\nprovider='fixture-provider'\nmodel='fixture-model'\nworkload='terminal-bench-2.1'\ntrial='{task}'\n[planning.repetition]\nindex=1\ntotal=1\n")).unwrap();
            if slot == 3 {
                config.planning.as_mut().unwrap().budgets.execution_seconds = Some(1200);
            }
            let mut policy = crate::planning::resolve(&config).unwrap();
            let task_default = 900.0 + slot as f64;
            policy.task_requirements = Some(crate::planning::TaskRequirements {
                source: "pinned-task-definition".into(),
                cpu_millis: Some(slot as u64 * 1000),
                memory_mib: Some(slot as u64 * 1024),
                execution_seconds: Some(task_default),
                evaluation_seconds: Some(300.0 + slot as f64),
                ..Default::default()
            });
            let caps = crate::planning::EffectiveBudgets {
                environment_preparation_seconds: Some(900),
                agent_preparation_seconds: None,
                execution_seconds: Some(if slot == 3 { 1200.0 } else { task_default }),
                evaluation_seconds: Some(1800),
                task_default_execution_seconds: Some(task_default),
            };
            let fingerprint =
                crate::planning::protocol_fingerprint(&config, &policy, caps, 1).unwrap();
            let altered = crate::planning::EffectiveBudgets {
                execution_seconds: Some(caps.execution_seconds.unwrap() + 1.0),
                ..caps
            };
            assert!(crate::planning::protocol_fingerprint(&config, &policy, altered, 1).is_err());
            let changed_ready = crate::planning::EffectiveBudgets {
                task_default_execution_seconds: Some(task_default + 1.0),
                execution_seconds: Some(if slot == 3 {
                    1200.0
                } else {
                    task_default + 1.0
                }),
                ..caps
            };
            assert!(
                crate::planning::protocol_fingerprint(&config, &policy, changed_ready, 1).is_err(),
                "a changed ready default cannot bypass the pinned task definition"
            );
            fingerprints.push(fingerprint.clone());
            let mut record = fixture.record(1, json!(true), true, true);
            record["summary"]["task"]["task_id"] = json!(task);
            record["summary"]["task"]["attempt_id"] = json!(format!("ab123-{slot}"));
            record["summary"]["execution_cohort"]["id"] = json!(format!("ab123{slot}"));
            record["summary"]["execution_plan_fingerprint"] = json!(fingerprint);
            record["summary"]["planning"] = json!(policy);
            record["summary"]["effective_budgets"] = json!(caps);
            fs::write(
                fixture.root.join(format!("{name}.toml")),
                toml::to_string(&config).unwrap(),
            )
            .unwrap();
            entries.push(json!({"reference":format!("{name}.toml"), "configuration":config, "policy":policy,
                "selection":{"workload":"terminal-bench-2.1","task_id":task,"dataset":fixture.dataset,
                    "evaluation_kind":"upstream-harness","population_size":3}}));
            let directory = fixture
                .root
                .join(format!("sweep-results/{index:03}-{name}/.asb-completions"));
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("001.json"), record.to_string()).unwrap();
        }
        fs::write(
            fixture.root.join("sweep-results/plan.json"),
            json!({"schema_version":1,"run_id":"a123","execution_mode":"replay","entries":entries})
                .to_string(),
        )
        .unwrap();
        assert_eq!(fingerprints[0], fingerprints[1]);
        assert_ne!(fingerprints[1], fingerprints[2]);
        let report = fixture.compare().unwrap();
        assert_eq!(report["metric_groups"].as_object().unwrap().len(), 2);
        assert_eq!(report["population_coverage"].as_object().unwrap().len(), 2);
        assert_eq!(report["resolution_groups"].as_object().unwrap().len(), 2);
        let mut counts = report["population_coverage"]
            .as_object()
            .unwrap()
            .values()
            .map(|group| group["selected_attempts"].as_u64().unwrap())
            .collect::<Vec<_>>();
        counts.sort_unstable();
        assert_eq!(counts, [1, 2]);
    }

    #[test]
    fn recorded_protocol_revisions_partition_scores_without_live_relabeling() {
        let fixture = Fixture::new("terminal-bench-2.1", 2);
        for slot in 1..=2 {
            let mut record = fixture.record(slot, json!(true), true, true);
            let mut module = crate::workloads::descriptor("terminal-bench-2.1").unwrap();
            module.dataset = Some(serde_json::from_value(fixture.dataset.clone()).unwrap());
            module.protocol.revision = slot as u32;
            record["summary"]["module"] = json!(module);
            fixture.publish(slot, &record);
        }
        let report = fixture.compare().unwrap();
        assert_eq!(report["metric_groups"].as_object().unwrap().len(), 2);
        assert_eq!(report["resolution_groups"].as_object().unwrap().len(), 2);
        assert_eq!(report["benchmarks"][0]["module"]["protocol"]["revision"], 1);
        assert_eq!(report["benchmarks"][1]["module"]["protocol"]["revision"], 2);
    }

    #[test]
    fn native_partial_rewards_survive_completion_collection_and_html_report() {
        use crate::workloads::outcome::*;
        let fixture = Fixture::new("terminal-bench-2.1", 4);
        let mut module = crate::workloads::descriptor("terminal-bench-2.1").unwrap();
        module.dataset = Some(serde_json::from_value(fixture.dataset.clone()).unwrap());
        module.protocol.score_policy = "metric:accuracy@2".into();
        assert!(module.valid());
        let plan_path = fixture.root.join("sweep-results/plan.json");
        let mut plan = read_json(&plan_path).unwrap();
        plan["entries"][0]["selection"]["module"] = json!(module);
        fs::write(plan_path, plan.to_string()).unwrap();
        let evidence = fixture.root.join("verifier.json");
        fs::write(&evidence, "{}").unwrap();
        for slot in 1..=3 {
            let mut record = fixture.record(slot, Value::Null, true, true);
            record["summary"]["module"] = json!(module);
            let evaluation = if slot == 3 {
                EvaluationOutcome::Pending {
                    reason: "external evaluator queued".into(),
                }
            } else {
                EvaluationOutcome::Completed {
                    resolution: None,
                    rewards: BTreeMap::from([("accuracy".into(), slot as f64 / 4.0)]),
                    reason: None,
                }
            };
            record["summary"]["benchmark_outcome"] = json!(BenchmarkOutcome {
                schema_version: 1,
                execution_success: Some(true),
                evaluation,
                cleanup_confirmed: Some(true),
                evidence: vec![evidence.clone()],
                policy: MetricPolicy {
                    id: "accuracy".into(),
                    version: 2,
                    reward: "accuracy".into(),
                    aggregation: Aggregation::TaskMean,
                    missing_rewards: MissingRewardPolicy::Exclude,
                    rewards: BTreeMap::from([(
                        "accuracy".into(),
                        RewardSpec {
                            unit: "ratio".into(),
                            range: Some([0.0, 1.0])
                        }
                    )])
                },
                lineage: EvaluationLineage {
                    execution_attempt: format!("ab123-{slot}"),
                    evaluation_id: format!("eval-{slot}"),
                    supersedes: None
                },
            });
            fixture.publish(slot, &record);
        }
        let report = fixture.compare().unwrap();
        let rows = report["benchmarks"].as_array().unwrap();
        assert!(rows[0]["metrics"]["success_rate"].is_null());
        assert_eq!(
            rows[0]["metrics"]["benchmark_rewards.accuracy@2.accuracy.ratio"],
            0.25
        );
        assert_eq!(report["evaluation_coverage"]["evaluated_attempts"], 2);
        let groups = report["metric_groups"].as_object().unwrap();
        assert_eq!(groups.len(), 1);
        let group = groups.values().next().unwrap();
        assert_eq!(group["estimate"], 0.375);
        assert_eq!(group["policy"]["missing_rewards"], "exclude");
        assert_eq!(group["repetitions_by_task"]["original-task"], 4);
        assert_eq!(group["selected_attempts"], 4);
        assert_eq!(group["complete"], false);
        assert!(group["task_cluster_standard_error"].is_null());
        let html =
            fs::read_to_string(fixture.root.join("results/sweep-comparison/report.html")).unwrap();
        assert!(html.contains("accuracy v2 / accuracy"));
        assert!(html.contains("0.375000"));
        assert!(html.contains("observed-only rewards; missing excluded"));
        let mut bad = fixture.record(1, Value::Null, true, true);
        bad["summary"]["benchmark_outcome"] = rows[0]["benchmark_outcome"].clone();
        bad["summary"]["benchmark_outcome"]["lineage"]["execution_attempt"] = json!("foreign");
        fixture.publish(1, &bad);
        assert!(fixture.compare().is_err());
    }

    #[test]
    fn every_original_adapter_keeps_unmeasured_grades_and_incomplete_population_honest() {
        for workload in [
            "swe-bench",
            "swe-bench-lite",
            "swe-bench-verified",
            "terminal-bench-1.0",
            "terminal-bench-2.1",
        ] {
            let fixture = Fixture::new(workload, 4);
            for (slot, grade, process, cleanup) in [
                (1, json!(true), false, false),
                (2, json!(false), true, true),
                (3, Value::Null, true, true),
            ] {
                fixture.publish(slot, &fixture.record(slot, grade, process, cleanup));
            }
            fs::write(
                fixture.directory().join(".asb-completions/.unfinished.tmp"),
                "incomplete record",
            )
            .unwrap();
            let report = fixture.compare().unwrap();
            let rows = report["benchmarks"].as_array().unwrap();
            assert_eq!(rows.len(), 4);
            for (slot, row) in rows.iter().enumerate().take(3) {
                assert!(
                    row["module"].is_null(),
                    "legacy records must not acquire today's descriptor"
                );
                assert_eq!(row["result_source"], "task-outcome");
                assert_eq!(row["replica_index"], slot + 1);
                assert_eq!(row["metrics"]["elapsed_ms"], 57.0);
                assert_eq!(row["metrics"]["token_usage.total_tokens"], 23.0);
                assert!(row["host_metadata"].is_null());
                assert!(row["components"].is_null());
                assert!(row["series"].as_object().unwrap().is_empty());
            }
            assert_eq!(rows[0]["metrics"]["success_rate"], 1.0);
            assert_eq!(rows[1]["metrics"]["success_rate"], 0.0);
            assert_eq!(rows[2]["metrics"]["success_rate"], 0.0);
            assert!(
                rows[0]["status"]
                    .as_str()
                    .unwrap()
                    .contains("cleanup not confirmed")
            );
            assert!(
                rows[0]["status"]
                    .as_str()
                    .unwrap()
                    .contains("fixture agent exited nonzero")
            );
            assert!(
                rows[2]["status"]
                    .as_str()
                    .unwrap()
                    .contains("fixture grader timed out")
            );
            assert_eq!(rows[3]["result_source"], "missing");
            assert!(rows[3]["task"].is_null());
            assert_eq!(report["evaluation_coverage"]["selected_attempts"], 4);
            assert_eq!(report["evaluation_coverage"]["evaluated_attempts"], 2);
            assert_eq!(report["evaluation_coverage"]["unevaluated_attempts"], 2);
            let metric = report["metric_groups"]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap();
            assert_eq!(
                metric["estimate"], 0.25,
                "binary metric must retain the selected-attempt denominator"
            );
            assert_eq!(metric["rewarded_attempts"], 2);
            assert_eq!(metric["contributing_attempts"], 4);
            assert_eq!(metric["policy"]["missing_rewards"], "zero");
            assert_eq!(metric["complete"], false);
            let group = report["resolution_groups"]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap();
            assert_eq!(group["task_count"], 4);
            assert_eq!(group["resolved_count"], 1);
            assert_eq!(group["success_rate"], 0.25);
            let population = report["population_coverage"]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap();
            assert!(
                !population["complete_population_evaluation"]
                    .as_bool()
                    .unwrap()
            );
            assert!(population["full_population_resolution_rate"].is_null());
            let html =
                fs::read_to_string(fixture.root.join("results/sweep-comparison/report.html"))
                    .unwrap();
            assert!(html.contains("optional measurements unavailable"));
            assert!(html.contains("metric-success_rate.svg"));
        }
    }

    #[test]
    fn termination_report_selects_only_agreed_current_attempt_facts_without_metric_graphs() {
        let known = json!({"kind":"signal","signal":15});
        for (retained_fact, measured_fact, diagnostic) in [
            (known.clone(), Value::Null, false),
            (Value::Null, known.clone(), false),
            (known.clone(), Value::Null, true),
            (Value::Null, Value::Null, false),
        ] {
            let fixture = Fixture::new("swe-bench-lite", 1);
            let mut record = fixture.record(1, json!(false), true, true);
            record["summary"]["process_termination"] = retained_fact.clone();
            let outcome = fixture.publish(1, &record);
            let directory = fixture.directory().join("measurements");
            fs::create_dir(&directory).unwrap();
            let measured = directory.join("run.json");
            let mut measurement = record["summary"].clone();
            measurement["status"] = json!("completed");
            measurement["success"] = json!(true);
            measurement["process_termination"] = measured_fact.clone();
            if diagnostic {
                measurement["process_termination_diagnostic"] = json!("malformed");
            }
            fs::write(&measured, measurement.to_string()).unwrap();
            let original_outcome = fs::read(&outcome).unwrap();
            let original_measurement = fs::read(&measured).unwrap();
            let report = fixture.compare().unwrap();
            let row = &report["benchmarks"][0];
            assert_eq!(
                row["process_termination"],
                if retained_fact.is_null() && measured_fact.is_null() {
                    Value::Null
                } else {
                    known.clone()
                }
            );
            assert_eq!(row["metrics"]["success_rate"], 0.0);
            assert!(
                !row["metrics"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|key| key.starts_with("process_termination"))
            );
            let graphs = fixture.root.join("results/sweep-comparison");
            assert!(!fs::read_dir(graphs).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("process_termination")
            }));
            assert_eq!(fs::read(&outcome).unwrap(), original_outcome);
            assert_eq!(fs::read(&measured).unwrap(), original_measurement);
            if !retained_fact.is_null() {
                let report_dir = fixture.root.join("results/sweep-comparison");
                let html = fs::read(report_dir.join("report.html")).unwrap();
                let json = fs::read(report_dir.join("comparison.json")).unwrap();
                let mut older_attempt = measurement.clone();
                older_attempt["process_termination"] = Value::Null;
                older_attempt
                    .as_object_mut()
                    .unwrap()
                    .remove("process_termination_diagnostic");
                older_attempt["task"]["attempt_id"] = json!("different-older-attempt");
                let bytes = serde_json::to_vec(&older_attempt).unwrap();
                fs::write(&measured, &bytes).unwrap();
                assert!(fixture.compare().is_err());
                assert_eq!(fs::read(&outcome).unwrap(), original_outcome);
                assert_eq!(fs::read(&measured).unwrap(), bytes);
                assert_eq!(fs::read(report_dir.join("report.html")).unwrap(), html);
                assert_eq!(fs::read(report_dir.join("comparison.json")).unwrap(), json);
            }
        }
    }

    #[test]
    fn retained_and_measured_results_must_agree_but_neither_changes_the_other() {
        let fixture = Fixture::new("swe-bench-lite", 1);
        let record = fixture.record(1, json!(true), true, true);
        let outcome = fixture.publish(1, &record);
        let directory = fixture.directory().join("measurements");
        fs::create_dir(&directory).unwrap();
        let measured = directory.join("run.json");
        let mut measurement = record["summary"].clone();
        measurement["status"] = json!("completed");
        measurement["success"] = json!(true);
        fs::write(&measured, measurement.to_string()).unwrap();
        assert_eq!(
            fixture.compare().unwrap()["benchmarks"][0]["result_source"],
            "measurements"
        );
        let before = fs::read(&outcome).unwrap();
        for field in [
            "workload_success",
            "task",
            "workload_baseline",
            "token_usage",
            "execution_cohort",
        ] {
            let mut changed = measurement.clone();
            changed[field] = json!("contradictory fixture");
            fs::write(&measured, changed.to_string()).unwrap();
            let error = fixture.compare().unwrap_err().to_string();
            assert!(error.contains(&format!("disagree on `{field}`")), "{error}");
            assert_eq!(fs::read(&outcome).unwrap(), before);
            assert_eq!(read_json(&measured).unwrap(), changed);
        }
        fs::remove_file(&measured).unwrap();
        assert_eq!(
            fixture.compare().unwrap()["benchmarks"][0]["result_source"],
            "task-outcome"
        );
    }

    #[test]
    fn complete_distinct_task_population_keeps_its_score_without_collectors() {
        for workload in [
            "swe-bench",
            "swe-bench-lite",
            "swe-bench-verified",
            "terminal-bench-1.0",
            "terminal-bench-2.1",
        ] {
            let fixture = Fixture::new(workload, 1);
            fixture.publish(1, &fixture.record(1, json!(false), true, true));
            let text = fs::read_to_string(fixture.root.join("one.toml"))
                .unwrap()
                .replace("original-task", "second-task");
            fs::write(fixture.root.join("two.toml"), &text).unwrap();
            fs::write(
                fixture.root.join("sweep.toml"),
                "configs=['one.toml','two.toml']\n",
            )
            .unwrap();
            let mut plan = read_json(&fixture.root.join("sweep-results/plan.json")).unwrap();
            plan["entries"][0]["selection"]["population_size"] = json!(2);
            let mut second = plan["entries"][0].clone();
            second["reference"] = json!("two.toml");
            second["configuration"] =
                json!(toml::from_str::<crate::benchmark::BenchmarkConfig>(&text).unwrap());
            second["selection"]["task_id"] = json!("second-task");
            plan["entries"].as_array_mut().unwrap().push(second);
            fs::write(
                fixture.root.join("sweep-results/plan.json"),
                plan.to_string(),
            )
            .unwrap();
            let directory = fixture.root.join("sweep-results/001-two/.asb-completions");
            fs::create_dir_all(&directory).unwrap();
            let mut second = fixture.record(1, json!(true), false, false);
            second["summary"]["task"]["task_id"] = json!("second-task");
            second["summary"]["task"]["attempt_id"] = json!("ab124-1");
            fs::write(directory.join("second.json"), second.to_string()).unwrap();
            let report = fixture.compare().unwrap();
            assert_eq!(report["evaluation_coverage"]["evaluated_attempts"], 2);
            let population = report["population_coverage"]
                .as_object()
                .unwrap()
                .values()
                .next()
                .unwrap();
            assert_eq!(population["selected_tasks"], 2);
            assert_eq!(population["resolved_tasks"], 1);
            assert_eq!(population["complete_population_evaluation"], true);
            assert_eq!(population["full_population_resolution_rate"], 0.5);
        }
    }

    #[test]
    fn fallback_rejects_mismatched_task_dataset_replica_and_duplicate_attempts() {
        let fixture = Fixture::new("swe-bench-lite", 2);
        let original = fixture.record(1, json!(true), true, true);
        for field in ["agent", "provider", "model", "workload"] {
            let mut changed = original.clone();
            changed["summary"][field] = json!("another-selection");
            fixture.publish(1, &changed);
            assert!(
                fixture
                    .compare()
                    .unwrap_err()
                    .to_string()
                    .contains("does not match selected")
            );
        }
        for mutate in 0..5 {
            let mut changed = original.clone();
            match mutate {
                0 => changed["summary"]["task"]["dataset"]["revision"] = json!("b".repeat(40)),
                1 => changed["summary"]["task"]["task_id"] = json!("another-task"),
                2 => changed["replica"]["total"] = json!(1),
                3 => changed["summary"]["execution_cohort"]["admitted_slots"] = json!([2]),
                _ => changed["summary"]["replica_index"] = json!(2),
            }
            fixture.publish(1, &changed);
            assert!(fixture.compare().is_err());
        }
        fixture.publish(1, &original);
        let mut second = fixture.record(2, json!(false), true, true);
        second["summary"]["task"]["attempt_id"] = original["summary"]["task"]["attempt_id"].clone();
        fixture.publish(2, &second);
        assert!(
            fixture
                .compare()
                .unwrap_err()
                .to_string()
                .contains("duplicate actual attempt")
        );
    }

    #[test]
    fn settlement_diagnostics_distinguish_unconfirmed_cleanup_from_valid_unknown_state() {
        let fixture = Fixture::new("swe-bench-lite", 1);
        let mut record = fixture.record(1, json!(true), true, true);
        record["workload_resource_disposition"] = json!("removed");
        record["summary"]["workload_resource_disposition"] = json!("removed");
        fixture.publish(1, &record);
        assert!(load(&fixture.directory(), 1).is_ok());
        record["workload_cleanup_confirmed"] = json!(false);
        fixture.publish(1, &record);
        assert!(
            load(&fixture.directory(), 1)
                .err()
                .unwrap()
                .to_string()
                .contains("outcome workload or proxy cleanup is unconfirmed")
        );
        record["workload_resource_disposition"] = Value::Null;
        record["summary"]["workload_resource_disposition"] = Value::Null;
        fixture.publish(1, &record);
        let observed = load(&fixture.directory(), 1).unwrap();
        assert!(!observed[&1].workload_cleanup);
        record["workload_cleanup_confirmed"] = json!(true);
        fixture.publish(1, &record);
        assert!(
            load(&fixture.directory(), 1)
                .err()
                .unwrap()
                .to_string()
                .contains("resource disposition contradicts cleanup confirmation")
        );
    }

    #[test]
    fn outcomes_are_bounded_regular_files_and_legacy_slots_must_be_unambiguous() {
        let fixture = Fixture::new("swe-bench-lite", 1);
        let original = fixture.record(1, json!(true), true, true);
        let path = fixture.publish(1, &original);
        let mut legacy = original.clone();
        legacy["schema_version"] = json!(1);
        legacy.as_object_mut().unwrap().remove("replica");
        legacy
            .as_object_mut()
            .unwrap()
            .remove("proxy_cleanup_confirmed");
        legacy["summary"]["replica_index"] = Value::Null;
        legacy["summary"]["execution_cohort"] = Value::Null;
        fixture.publish(1, &legacy);
        assert_eq!(load(&fixture.directory(), 1).unwrap().len(), 1);
        let report = fixture.compare().unwrap();
        assert_eq!(report["benchmarks"][0]["metrics"]["success_rate"], 1.0);
        assert!(
            report["benchmarks"][0]["status"]
                .as_str()
                .unwrap()
                .contains("cleanup not confirmed")
        );
        assert!(load(&fixture.directory(), 2).is_err());
        fixture.publish(1, &original);
        let duplicate = fixture.directory().join(".asb-completions/duplicate.json");
        fs::copy(&path, &duplicate).unwrap();
        assert!(load(&fixture.directory(), 1).is_err());
        fs::remove_file(&duplicate).unwrap();
        fs::write(&path, vec![b' '; LIMIT as usize + 1]).unwrap();
        assert!(load(&fixture.directory(), 1).is_err());
        fs::write(&path, "{").unwrap();
        assert!(load(&fixture.directory(), 1).is_err());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(load(&fixture.directory(), 1).is_err());
        fs::remove_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, symlink};
            symlink(fixture.root.join("one.toml"), &path).unwrap();
            assert!(load(&fixture.directory(), 1).is_err());
            fs::remove_file(&path).unwrap();
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(fs::symlink_metadata(&path).unwrap().file_type().is_fifo());
            assert!(load(&fixture.directory(), 1).is_err());
        }
    }

    #[test]
    fn legacy_multi_replica_outcomes_map_only_by_actual_measured_attempt() {
        let fixture = Fixture::new("swe-bench-lite", 2);
        for slot in 1..=2 {
            let mut record = fixture.record(slot, json!(slot == 1), true, true);
            record["schema_version"] = json!(1);
            record.as_object_mut().unwrap().remove("replica");
            record["summary"]["replica_index"] = Value::Null;
            record["summary"]["execution_cohort"] = Value::Null;
            fixture.publish(slot, &record);
            let directory = fixture
                .directory()
                .join(format!("measurements-concurrency-{slot}"));
            fs::create_dir(&directory).unwrap();
            let mut measured = record["summary"].clone();
            measured["status"] = json!("completed");
            fs::write(directory.join("run.json"), measured.to_string()).unwrap();
        }
        let report = fixture.compare().unwrap();
        assert_eq!(report["benchmarks"][0]["metrics"]["success_rate"], 1.0);
        assert_eq!(report["benchmarks"][1]["metrics"]["success_rate"], 0.0);
        let records = fixture.directory().join(".asb-completions");
        fs::rename(
            records.join("001.json"),
            records.join("z-first-attempt.json"),
        )
        .unwrap();
        fs::rename(
            records.join("002.json"),
            records.join("a-second-attempt.json"),
        )
        .unwrap();
        let reordered = fixture.compare().unwrap();
        assert_eq!(
            reordered, report,
            "filenames may order validation but cannot choose replica identity"
        );
        fs::rename(
            records.join("z-first-attempt.json"),
            records.join("001.json"),
        )
        .unwrap();
        fs::rename(
            records.join("a-second-attempt.json"),
            records.join("002.json"),
        )
        .unwrap();
        fs::copy(
            fixture
                .directory()
                .join("measurements-concurrency-1/run.json"),
            fixture
                .directory()
                .join("measurements-concurrency-2/run.json"),
        )
        .unwrap();
        assert!(
            fixture
                .compare()
                .unwrap_err()
                .to_string()
                .contains("multiple measured replica slots")
        );
    }

    #[test]
    fn ineligible_retained_evaluation_never_claims_resolution() {
        let fixture = Fixture::new("swe-bench-lite", 1);
        for kind in ["adapter-smoke", "upstream-harness", "unknown"] {
            let mut record = fixture.record(1, json!(true), true, true);
            record["summary"]["workload_evaluation"] = json!(kind);
            record["summary"]["workload_baseline"] = json!({"kind":"unavailable"});
            fixture.publish(1, &record);
            let report = fixture.compare().unwrap();
            let row = &report["benchmarks"][0];
            assert_eq!(row["evaluation_completed"], false);
            assert!(row["metrics"].get("success_rate").is_none());
            assert!(!row["status"].as_str().unwrap().contains("task resolved"));
        }
    }

    #[test]
    fn legacy_comparison_without_a_plan_still_bounds_replica_population() {
        let fixture = Fixture::new("swe-bench-lite", 1);
        fs::remove_file(fixture.root.join("sweep-results/plan.json")).unwrap();
        let text = fs::read_to_string(fixture.root.join("one.toml")).unwrap();
        let above_limit = (crate::benchmark::MAX_CONCURRENCY + 1).to_string();
        for value in ["0", above_limit.as_str(), "-1", "'unbounded'"] {
            fs::write(
                fixture.root.join("one.toml"),
                text.replace("concurrency=1", &format!("concurrency={value}")),
            )
            .unwrap();
            assert!(
                fixture
                    .compare()
                    .unwrap_err()
                    .to_string()
                    .contains("concurrency must be an integer")
            );
        }
    }
}
