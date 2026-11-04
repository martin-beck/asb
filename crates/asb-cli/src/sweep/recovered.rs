//! Explicit grade-only reporting recovery. Never an execution/resume receipt.
use super::checkpoint::completed::{allocations, publish, read_private};
use super::plan::{PlannedBenchmark, SweepPlan};
use crate::runner::replica::Identity;
use crate::workloads::recovery::{AttemptReference, inspect_reference};
use crate::{Error, io_error};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

const SCOPE: &str = "imported-official-grade-not-execution-not-resume";

#[cfg(test)]
mod tests;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Imported {
    schema_version: u32,
    scope: String,
    configuration: usize,
    replica: Identity,
    reference: AttemptReference,
    binding: Value,
    report: Value,
}

fn invalid(message: &str) -> Error {
    Error::Input(format!("retained grade import: {message}"))
}

fn original_binding(
    checkpoint: &Path,
    index: usize,
    replica: Identity,
    directory: &Path,
    entry: &PlannedBenchmark,
) -> Result<(AttemptReference, Value), Error> {
    let reference = allocations::reporting_reference(checkpoint, index, replica, directory, entry)?;
    let relative = format!("{index:06}/{:03}.json", replica.index);
    let allocation: Value = read_private(&checkpoint.join("allocated").join(&relative))?;
    let intent: Value = read_private(&checkpoint.join("started").join(relative))?;
    Ok((reference, json!({"allocation":allocation,"intent":intent})))
}

pub(super) fn reporting_summary(entry: &PlannedBenchmark, report: &Value) -> Value {
    use sha2::{Digest, Sha256};
    let provenance = crate::workloads::outcome::LegacyEvaluationProvenance {
        execution_attempt: report["task"]["attempt_id"].as_str().map(str::to_owned),
        source_scope: report["scope"].as_str().map(str::to_owned),
        evidence_manifest_sha256: report
            .get("evidence")
            .map(|evidence| format!("{:x}", Sha256::digest(evidence.to_string().as_bytes()))),
        evaluation_id: None,
        supersedes: None,
    };
    let mut value = report.clone();
    let object = value.as_object_mut().expect("validated inspected grade");
    object.remove("evidence");
    // Official-grade inspection is not an observation of the agent process.
    // Never carry execution facts forward from an extensible grading record.
    object.insert("process_termination".into(), Value::Null);
    object.remove("process_termination_diagnostic");
    object.insert("legacy_evaluation_provenance".into(), json!(provenance));
    // Schema 2 controls scoring; this is not a RunSummary and lacks a cohort
    // because no execution observations are being recovered.
    object.insert("schema_version".into(), json!(2));
    for (field, text) in [
        ("agent", &entry.configuration.agent),
        ("workload", &entry.configuration.workload),
        ("provider", &entry.configuration.provider),
        ("model", &entry.configuration.model),
    ] {
        object.insert(field.into(), json!(text));
    }
    value
}

/// Revalidate the original binding and adapter-owned grading artifacts whenever
/// reporting uses an import. Edited or now-active attempts cannot earn a grade
/// from an old copy. This starts SDK metadata inspection, not Docker/evaluation.
pub(crate) fn load(
    checkpoint: &Path,
    directory: &Path,
    index: usize,
    entry: &PlannedBenchmark,
) -> Result<BTreeMap<usize, (PathBuf, Value)>, Error> {
    let mut records = BTreeMap::new();
    let imported = directory.join(".asb-imported-grades");
    match std::fs::symlink_metadata(&imported) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(records),
        Err(error) => return Err(io_error(error)),
        Ok(metadata) if !metadata.is_dir() => {
            return Err(invalid("import storage must be a real directory"));
        }
        Ok(_) => (),
    }
    let mut population = 0;
    for item in std::fs::read_dir(&imported).map_err(io_error)? {
        population += 1;
        if population > 2 * crate::benchmark::MAX_CONCURRENCY {
            return Err(invalid("import population exceeds its bound"));
        }
        let path = item.map_err(io_error)?.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with('.') && name.ends_with(".tmp"))
        {
            continue;
        }
        let record: Imported = read_private(&path)?;
        if record.schema_version != 1
            || record.scope != SCOPE
            || record.configuration != index
            || !record.replica.valid()
            || record.replica.total != entry.configuration.concurrency
            || path.file_name().and_then(|name| name.to_str())
                != Some(format!("{:03}.json", record.replica.index).as_str())
        {
            return Err(invalid(
                "import has an invalid scope, configuration or slot",
            ));
        }
        let (reference, binding) =
            original_binding(checkpoint, index, record.replica, directory, entry)?;
        if reference != record.reference
            || binding != record.binding
            || inspect_reference(&reference)? != record.report
        {
            return Err(invalid(
                "original allocation, start intent or official grade changed; preserve the import and inspect its source",
            ));
        }
        let value = reporting_summary(entry, &record.report);
        if records
            .insert(record.replica.index, (path, value))
            .is_some()
        {
            return Err(invalid("multiple imported grades claim the same slot"));
        }
    }
    Ok(records)
}

pub(super) fn import(
    sweep_path: &Path,
    manifest: &super::SweepFile,
    parent: &Path,
    output_parent: &Path,
    stem: &str,
    json_output: bool,
    output: &mut dyn Write,
) -> Result<(), Error> {
    let results = output_parent.join(format!("{stem}-results"));
    if !std::fs::symlink_metadata(&results).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(invalid("original sweep results must be a real directory"));
    }
    let plan = SweepPlan::load(&results, parent, manifest)?
        .ok_or_else(|| invalid("original sweep plan is missing"))?;
    let checkpoint = results.join(".checkpoint");
    let mut pending = Vec::new();
    let mut bytes = 0usize;
    let mut unchanged = 0usize;
    let mut attempts = BTreeSet::new();
    // Validate the entire selected population before publishing any record.
    for (index, entry) in plan.entries.iter().enumerate() {
        let directory = results.join(super::execution::result_name(
            index,
            Path::new(&entry.reference),
        ));
        let outcomes = crate::compare::outcomes::load(&directory, entry.configuration.concurrency)?;
        let existing = load(&checkpoint, &directory, index, entry)?;
        for value in outcomes
            .values()
            .map(|outcome| &outcome.summary)
            .chain(existing.values().map(|(_, value)| value))
        {
            claim_attempt(&mut attempts, value)?;
        }
        for slot in 1..=entry.configuration.concurrency {
            let path = directory
                .join(".asb-imported-grades")
                .join(format!("{slot:03}.json"));
            if existing.contains_key(&slot) {
                if outcomes.contains_key(&slot)
                    || measurement(&directory, slot, entry.configuration.concurrency).exists()
                {
                    return Err(invalid(
                        "an imported slot also has an execution outcome or measurement; preserve both and inspect the conflict",
                    ));
                }
                unchanged += 1;
                continue;
            }
            // Already recorded outcomes are authoritative, never replaced by
            // an import. Unstarted/native slots with no allocation are ignored.
            if outcomes.contains_key(&slot) {
                continue;
            }
            let allocation = checkpoint
                .join("allocated")
                .join(format!("{index:06}/{slot:03}.json"));
            match std::fs::symlink_metadata(&allocation) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io_error(error)),
                Ok(_) => (),
            }
            if measurement(&directory, slot, entry.configuration.concurrency).exists() {
                return Err(invalid(
                    "a retained slot has a measurement record; preserve it rather than replacing or guessing its grade",
                ));
            }
            let replica = Identity {
                index: slot,
                total: entry.configuration.concurrency,
            };
            let (reference, binding) =
                original_binding(&checkpoint, index, replica, &directory, entry)?;
            let report = inspect_reference(&reference)?;
            claim_attempt(&mut attempts, &report)?;
            let record = Imported {
                schema_version: 1,
                scope: SCOPE.into(),
                configuration: index,
                replica,
                reference,
                binding,
                report,
            };
            bytes = bytes.saturating_add(
                serde_json::to_vec(&record)
                    .map_err(|_| invalid("cannot encode import"))?
                    .len(),
            );
            if bytes > 64 * 1024 * 1024 {
                return Err(invalid(
                    "pending grade imports exceed 64 MiB; import a smaller original sweep",
                ));
            }
            pending.push((path, record));
        }
    }
    for (path, record) in &pending {
        publish(path, record)?;
    }
    if json_output {
        writeln!(output, "{}", json!({"schema_version":1,"scope":SCOPE,"imported":pending.len(),"unchanged":unchanged,"execution_recovered":false,"resume_receipts_created":false})).map_err(io_error)?;
    } else {
        writeln!(output, "Imported {} official task grades; {} existing imports unchanged. Execution metrics remain unknown. No agents/evaluators ran and no resume receipts were created. Run `asb compare {}` to regenerate reports.", pending.len(), unchanged, sweep_path.display()).map_err(io_error)?;
    }
    Ok(())
}

fn claim_attempt(attempts: &mut BTreeSet<String>, value: &Value) -> Result<(), Error> {
    if let Some(attempt) = value["task"]["attempt_id"].as_str()
        && !attempts.insert(attempt.into())
    {
        return Err(invalid(
            "the same actual attempt claims multiple selected slots",
        ));
    }
    Ok(())
}

fn measurement(directory: &Path, slot: usize, total: usize) -> PathBuf {
    directory
        .join(if total == 1 {
            "measurements".into()
        } else {
            format!("measurements-concurrency-{slot}")
        })
        .join("run.json")
}
