//! Explicit recovery for an otherwise successful slot lost only to the old
//! perf-stat settlement deadline. This preserves evidence before ordinary
//! continuation re-admits the slot.
use super::*;
use crate::sweep::checkpoint::Snapshotter;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;

const SCOPE: &str = "collector-teardown-recovery-v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    scope: String,
    status: String,
    configuration: usize,
    slot: u64,
    input_digest: String,
    archive: PathBuf,
    fingerprint: Option<Fingerprint>,
}

fn receipt_relative(configuration: usize, slot: u64) -> PathBuf {
    PathBuf::from(format!(
        "collector-teardown-recovery/{configuration:06}/{slot:06}.json"
    ))
}

fn archive_relative(slot: &crate::runner::campaign::Slot) -> PathBuf {
    PathBuf::from(format!(".asb-recovery/collector-teardown/{:06}", slot.id))
}

fn receipt_path(checkpoint: &Path, configuration: usize, slot: u64) -> PathBuf {
    checkpoint.join(receipt_relative(configuration, slot))
}

fn archive_root(layout: &Layout, slot: &crate::runner::campaign::Slot) -> PathBuf {
    layout.root.join(archive_relative(slot))
}

fn context() -> Value {
    serde_json::json!({"scope": SCOPE})
}

/// A completed receipt removes a slot's original observations from ordinary
/// admission only after proving that their retained archive is intact.
pub(super) fn read_receipt(
    checkpoint: &Path,
    layout: &Layout,
    configuration: usize,
    slot: &crate::runner::campaign::Slot,
    input_digest: &str,
) -> Result<bool, Error> {
    let relative = receipt_relative(configuration, slot.id);
    if !reading::present_relative(checkpoint, &relative)? {
        return Ok(false);
    }
    let bytes = reading::bounded_relative(checkpoint, &relative, 64 * 1024)?;
    let receipt: Receipt = serde_json::from_slice(&bytes)
        .map_err(|_| recovery("collector-teardown recovery receipt is malformed"))?;
    if receipt.schema_version != 1
        || receipt.scope != SCOPE
        || receipt.configuration != configuration
        || receipt.slot != slot.id
        || receipt.input_digest != input_digest
        || receipt.archive != archive_relative(slot)
    {
        return Err(recovery(
            "collector-teardown recovery receipt differs from its original slot",
        ));
    }
    if receipt.status != "complete" {
        return Err(recovery(
            "collector-teardown recovery is incomplete; preserve the archive and repair it explicitly",
        ));
    }
    let fingerprint = receipt.fingerprint.ok_or_else(|| {
        recovery("collector-teardown recovery receipt has no archive fingerprint")
    })?;
    if !fingerprint.valid() {
        return Err(recovery(
            "collector-teardown recovery receipt has an invalid archive fingerprint",
        ));
    }
    let snapshotter = Snapshotter::open(checkpoint, PathBuf::from("python3"))?;
    let archive = archive_root(layout, slot);
    let actual = snapshotter.fingerprint(
        &[Root::path("collector-teardown-archive", archive)],
        &context(),
        false,
        &[],
    )?;
    if actual != fingerprint {
        return Err(recovery(
            "collector-teardown recovery archive differs from its receipt",
        ));
    }
    Ok(true)
}

fn write_receipt(path: &Path, receipt: &Receipt, replace: bool) -> Result<(), Error> {
    let bytes = serde_json::to_vec_pretty(receipt)
        .map_err(|_| recovery("cannot encode collector-teardown recovery receipt"))?;
    let parent = path
        .parent()
        .ok_or_else(|| recovery("collector-teardown receipt has no parent"))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    let temporary = path.with_extension("tmp");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(io_error)?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(io_error)?;
    if replace {
        fs::rename(&temporary, path).map_err(io_error)?;
    } else {
        match fs::hard_link(&temporary, path) {
            Ok(()) => fs::remove_file(&temporary).map_err(io_error)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&temporary);
                return Err(recovery(
                    "collector-teardown recovery receipt already exists",
                ));
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(io_error(error));
            }
        }
    }
    Ok(())
}

fn eligible(layout: &Layout, slot: &crate::runner::campaign::Slot) -> Result<(), Error> {
    if layout.outcome(slot)?.is_some() {
        return Err(recovery(
            "collector-teardown recovery requires a slot without an actual completion",
        ));
    }
    let measurements = layout
        .measurement_directory(slot)
        .ok_or_else(|| recovery("collector-teardown recovery requires retained measurements"))?;
    let summary: Value = serde_json::from_slice(&reading::bounded(
        &measurements.join("summary.json"),
        16 * 1024 * 1024,
    )?)
    .map_err(|_| recovery("collector-teardown measurement summary is malformed"))?;
    let run = summary["runs"]
        .as_array()
        .and_then(|runs| (runs.len() == 1).then(|| &runs[0]))
        .ok_or_else(|| recovery("collector-teardown recovery requires exactly one measured run"))?;
    let execution = serde_json::to_value(layout.plan.execution(slot))
        .map_err(|_| recovery("cannot encode collector-teardown execution"))?;
    if summary["success_count"] != 1
        || summary["failure_count"] != 0
        || summary["success_rate"] != 1.0
        || run["success"] != true
        || run["process_success"] != true
        || run["workload_success"] != true
        || run["exit_code"] != 0
        || run["execution"] != execution
        || run["benchmark_outcome"]["execution_success"] != true
        || run["benchmark_outcome"]["cleanup_confirmed"] != true
        || run["benchmark_outcome"]["evaluation"]["status"] != "completed"
        || run["benchmark_outcome"]["evaluation"]["resolution"] != true
    {
        return Err(recovery(
            "collector-teardown recovery requires an otherwise successful officially resolved slot",
        ));
    }
    let batch = layout.root.join(format!("batch-{}", slot.batch));
    let finished: Value =
        serde_json::from_slice(&reading::bounded(&batch.join("finished.json"), 64 * 1024)?)
            .map_err(|_| recovery("collector-teardown batch terminal record is malformed"))?;
    let reason = finished["failure_reason"]
        .as_str()
        .ok_or_else(|| recovery("collector-teardown batch has no failure reason"))?;
    let required = format!(
        "replica {}: cannot finish perf-stat collector: collector settlement deadline exceeded",
        slot.replica
    );
    if finished["schema_version"] != 1
        || finished["failed"] != true
        || !reason.contains(&required)
        || !reason.contains("raw capture has not reached its stopped boundary")
    {
        return Err(recovery(
            "collector-teardown batch failure is not the supported perf-stat settlement defect",
        ));
    }
    Ok(())
}

fn move_if_present(
    source: &Path,
    destination: &Path,
    moved: &mut BTreeSet<PathBuf>,
) -> Result<(), Error> {
    if !moved.insert(source.to_path_buf()) {
        return Ok(());
    }
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(error)),
    };
    if metadata.file_type().is_symlink() || destination.exists() {
        return Err(recovery(
            "collector-teardown archive source or destination is unsafe",
        ));
    }
    fs::create_dir_all(
        destination
            .parent()
            .ok_or_else(|| recovery("collector-teardown archive has no parent"))?,
    )
    .map_err(io_error)?;
    fs::rename(source, destination).map_err(|error| {
        Error::Input(format!(
            "cannot atomically archive collector-teardown evidence {}: {error}; preserve the original campaign and use one filesystem for campaign measurements",
            source.display()
        ))
    })
}

impl Inputs {
    pub(in crate::sweep) fn recover_collector_teardown_slot(
        &self,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
    ) -> Result<Value, Error> {
        let owned = self.own_campaigns(parent, results, plan)?;
        let inventory = Inventory::read(&owned.layouts, &self.directory, &self.initial.digest)?;
        let mut candidate = None;
        for (configuration, layout) in owned.layouts.iter().enumerate() {
            for slot in &layout.plan.slots {
                let key = (configuration, slot.id);
                if !inventory.observed.contains(&key) || inventory.actual.contains_key(&key) {
                    continue;
                }
                eligible(layout, slot)?;
                if candidate.replace((configuration, layout, slot)).is_some() {
                    return Err(recovery(
                        "collector-teardown recovery requires exactly one eligible observed slot",
                    ));
                }
            }
        }
        let (configuration, layout, slot) = candidate
            .ok_or_else(|| recovery("no eligible collector-teardown-only slot was found"))?;
        let receipt_path = receipt_path(&self.directory, configuration, slot.id);
        let archive = archive_root(layout, slot);
        if archive.exists() {
            return Err(recovery("collector-teardown archive already exists"));
        }
        let mut receipt = Receipt {
            schema_version: 1,
            scope: SCOPE.into(),
            status: "pending".into(),
            configuration,
            slot: slot.id,
            input_digest: self.initial.digest.clone(),
            archive: archive_relative(slot),
            fingerprint: None,
        };
        write_receipt(&receipt_path, &receipt, false)?;
        fs::create_dir_all(&archive).map_err(io_error)?;
        let batch = layout.root.join(format!("batch-{}", slot.batch));
        let mut moved = BTreeSet::new();
        move_if_present(
            &layout.root.join(&slot.workspace),
            &archive.join("workspace"),
            &mut moved,
        )?;
        move_if_present(
            &layout
                .root
                .join(crate::runner::replica::runtime_path(&slot.workspace)),
            &archive.join("runtime"),
            &mut moved,
        )?;
        move_if_present(
            &layout.root.join(&slot.log),
            &archive.join("log"),
            &mut moved,
        )?;
        for (source, name) in [
            (
                batch.join(format!("starts/{}.json", slot.replica)),
                "start.json",
            ),
            (
                batch.join(format!("allocations/{}.json", slot.replica)),
                "allocation.json",
            ),
            (
                batch.join(format!("outcomes/{}.json", slot.replica)),
                "outcome.json",
            ),
            (batch.join("finished.json"), "batch-finished.json"),
            (layout.root.join("finished.json"), "campaign-finished.json"),
        ] {
            move_if_present(&source, &archive.join(name), &mut moved)?;
        }
        if let Some(measurements) = layout.measurement_directory(slot) {
            move_if_present(&measurements, &archive.join("measurements"), &mut moved)?;
        }
        let producer = crate::runner::recording::producer(
            &layout.root,
            slot.batch,
            slot.replica,
            layout.plan.concurrency,
        );
        move_if_present(&producer, &archive.join("producer"), &mut moved)?;
        if let Some(destination) = &layout.plan.recording {
            let association = crate::runner::recording::association_for(destination, slot.id)?;
            move_if_present(
                &association,
                &archive.join("recording-association"),
                &mut moved,
            )?;
        }
        let marker = format!("-concurrency-{}", slot.replica);
        for entry in fs::read_dir(&batch).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == format!("measurements-concurrency-{}", slot.replica)
                || !(name.ends_with(&marker) || name.starts_with(&format!("{marker}.")))
                    && !name.contains(&format!("{marker}."))
            {
                continue;
            }
            move_if_present(&entry.path(), &archive.join("batch").join(name), &mut moved)?;
        }
        // The archive root contains exactly the retained roots, and binds any
        // retained file or directory without exposing its contents in output.
        let fingerprint = self.snapshotter.fingerprint(
            &[Root::path("collector-teardown-archive", archive.clone())],
            &context(),
            false,
            &[],
        )?;
        receipt.status = "complete".into();
        receipt.fingerprint = Some(fingerprint);
        write_receipt(&receipt_path, &receipt, true)?;
        Ok(serde_json::json!({
            "type":"collector-teardown-recovery",
            "configuration":configuration,
            "slot":slot.id,
            "agent_executions":0,
            "evaluations":0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep::checkpoint::campaigns::tests::settlement_tests;

    #[test]
    fn completed_receipt_requires_the_retained_archive_to_match() {
        let (_fixture, plan, results, inputs) =
            settlement_tests::fixture_concurrency(crate::tape::Mode::Replay, false, false, 1);
        let layout = plan.read_campaigns(&results).unwrap().remove(0);
        let slot = &layout.plan.slots[0];
        let archive = archive_root(&layout, slot);
        fs::create_dir_all(&archive).unwrap();
        fs::write(archive.join("retained.txt"), b"original evidence").unwrap();
        let fingerprint = inputs
            .snapshotter
            .fingerprint(
                &[Root::path("collector-teardown-archive", archive.clone())],
                &context(),
                false,
                &[],
            )
            .unwrap();
        let receipt = Receipt {
            schema_version: 1,
            scope: SCOPE.into(),
            status: "complete".into(),
            configuration: 0,
            slot: slot.id,
            input_digest: inputs.initial.digest.clone(),
            archive: archive_relative(slot),
            fingerprint: Some(fingerprint),
        };
        write_receipt(
            &receipt_path(&inputs.directory, 0, slot.id),
            &receipt,
            false,
        )
        .unwrap();
        assert!(read_receipt(&inputs.directory, &layout, 0, slot, &inputs.initial.digest).unwrap());
        fs::write(archive.join("retained.txt"), b"changed evidence").unwrap();
        assert!(read_receipt(&inputs.directory, &layout, 0, slot, &inputs.initial.digest).is_err());
    }
}
