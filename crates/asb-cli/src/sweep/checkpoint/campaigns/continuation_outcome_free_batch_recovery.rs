//! Explicit recovery for a fully failed batch that produced no actual outcome.
//! The complete failed batch is archived before ordinary continuation creates a
//! new batch directory, so no failed collector artifact is overwritten.
use super::*;
use crate::sweep::checkpoint::Snapshotter;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;

const SCOPE: &str = "outcome-free-batch-recovery-v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    scope: String,
    status: String,
    configuration: usize,
    batch: u64,
    input_digest: String,
    archive: PathBuf,
    fingerprint: Option<Fingerprint>,
}

fn receipt_relative(configuration: usize, batch: u64) -> PathBuf {
    PathBuf::from(format!(
        "outcome-free-batch-recovery/{configuration:06}/{batch:06}.json"
    ))
}

fn archive_relative(batch: u64) -> PathBuf {
    PathBuf::from(format!(".asb-recovery/outcome-free-batch/{batch:06}"))
}

fn archive_relative_next(layout: &Layout, batch: u64) -> PathBuf {
    let base = archive_relative(batch);
    if !layout.root.join(&base).exists() {
        return base;
    }
    for generation in 2..=u32::MAX {
        let candidate = PathBuf::from(format!(
            ".asb-recovery/outcome-free-batch/{batch:06}-{generation:06}"
        ));
        if !layout.root.join(&candidate).exists() {
            return candidate;
        }
    }
    PathBuf::from(format!(
        ".asb-recovery/outcome-free-batch/{batch:06}-overflow"
    ))
}

fn valid_archive_relative(path: &Path) -> bool {
    path.is_relative()
        && path.parent() == Some(Path::new(".asb-recovery/outcome-free-batch"))
        && path.file_name().is_some_and(|name| !name.is_empty())
}

fn receipt_path(checkpoint: &Path, configuration: usize, batch: u64) -> PathBuf {
    checkpoint.join(receipt_relative(configuration, batch))
}

fn receipt_history_path(
    checkpoint: &Path,
    configuration: usize,
    batch: u64,
    archive: &Path,
) -> Result<PathBuf, Error> {
    let name = archive
        .file_name()
        .ok_or_else(|| recovery("outcome-free batch recovery archive has no name"))?;
    Ok(checkpoint
        .join(format!("outcome-free-batch-recovery/{configuration:06}"))
        .join(format!("{batch:06}-{}.json", name.to_string_lossy())))
}

fn archive_root_from_receipt(layout: &Layout, archive: &Path) -> Result<PathBuf, Error> {
    if !valid_archive_relative(archive) {
        return Err(recovery(
            "outcome-free batch recovery archive path is invalid",
        ));
    }
    Ok(layout.root.join(archive))
}

fn context() -> Value {
    serde_json::json!({"scope": SCOPE})
}

fn read_receipt_value(
    checkpoint: &Path,
    configuration: usize,
    batch: u64,
    input_digest: &str,
) -> Result<Option<Receipt>, Error> {
    let relative = receipt_relative(configuration, batch);
    if !reading::present_relative(checkpoint, &relative)? {
        return Ok(None);
    }
    let bytes = reading::bounded_relative(checkpoint, &relative, 64 * 1024)?;
    let receipt: Receipt = serde_json::from_slice(&bytes)
        .map_err(|_| recovery("outcome-free batch recovery receipt is malformed"))?;
    if receipt.schema_version != 1
        || receipt.scope != SCOPE
        || !matches!(receipt.status.as_str(), "pending" | "complete")
        || receipt.configuration != configuration
        || receipt.batch != batch
        || receipt.input_digest != input_digest
        || !valid_archive_relative(&receipt.archive)
    {
        return Err(recovery(
            "outcome-free batch recovery receipt differs from its original batch",
        ));
    }
    Ok(Some(receipt))
}

fn real_directory(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

fn batch_roots(layout: &Layout, batch: u64) -> Result<Vec<PathBuf>, Error> {
    let mut roots = Vec::new();
    let legacy = layout.root.join(format!("batch-{batch}"));
    if real_directory(&legacy)? {
        roots.push(legacy);
    }
    let generations = Path::new(".asb-generations");
    if reading::present_relative(&layout.root, generations)? {
        for generation in reading::list_relative(&layout.root, generations, 65_533)? {
            let relative = generations.join(generation).join(format!("batch-{batch}"));
            let root = layout.root.join(relative);
            if real_directory(&root)? {
                roots.push(root);
            }
        }
    }
    Ok(roots)
}

pub(super) fn read_receipt(
    checkpoint: &Path,
    layout: &Layout,
    configuration: usize,
    batch: u64,
    input_digest: &str,
    snapshotter: &Snapshotter,
) -> Result<bool, Error> {
    let Some(receipt) = read_receipt_value(checkpoint, configuration, batch, input_digest)? else {
        return Ok(false);
    };
    if receipt.status == "pending" {
        return Err(recovery(
            "outcome-free batch recovery is incomplete; rerun --recover-outcome-free-batch",
        ));
    }
    let fingerprint = receipt.fingerprint.ok_or_else(|| {
        recovery("outcome-free batch recovery receipt has no archive fingerprint")
    })?;
    if !fingerprint.valid() {
        return Err(recovery(
            "outcome-free batch recovery receipt has an invalid archive fingerprint",
        ));
    }
    let archive = archive_root_from_receipt(layout, &receipt.archive)?;
    let actual = snapshotter.fingerprint(
        &[Root::path("outcome-free-batch-archive", archive)],
        &context(),
        false,
        &[],
    )?;
    if actual != fingerprint {
        return Err(recovery(
            "outcome-free batch recovery archive differs from its receipt",
        ));
    }
    // A later failed generation may reuse the same logical batch number after
    // an earlier outcome-free archive. Preserve that archive and let the new
    // source batch be recovered under a distinct archive path.
    Ok(batch_roots(layout, batch)?.is_empty())
}

fn write_receipt(path: &Path, receipt: &Receipt, replace: bool) -> Result<(), Error> {
    let bytes = serde_json::to_vec_pretty(receipt)
        .map_err(|_| recovery("cannot encode outcome-free batch recovery receipt"))?;
    let parent = path
        .parent()
        .ok_or_else(|| recovery("outcome-free batch recovery receipt has no parent"))?;
    fs::create_dir_all(parent).map_err(io_error)?;
    let temporary = path.with_extension("tmp");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(&temporary) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            fs::remove_file(&temporary).map_err(io_error)?;
            options.open(&temporary).map_err(io_error)?
        }
        Err(error) => return Err(io_error(error)),
    };
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
                    "outcome-free batch recovery receipt already exists",
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

fn terminal_record_relative(
    layout: &Layout,
    batch: u64,
    generation: Option<&str>,
) -> Result<PathBuf, Error> {
    let legacy = PathBuf::from(format!("batch-{batch}/finished.json"));
    if let Some(generation) = generation {
        let generated = PathBuf::from(".asb-generations")
            .join(generation)
            .join(format!("batch-{batch}"))
            .join("finished.json");
        if reading::present_relative(&layout.root, &generated)? {
            return Ok(generated);
        }
        return Err(recovery(
            "outcome-free batch recovery has no catalog-authorized terminal record",
        ));
    }
    if reading::present_relative(&layout.root, &legacy)? {
        return Ok(legacy);
    }
    Err(recovery(
        "outcome-free batch recovery has no terminal record",
    ))
}

fn batch_root_from_terminal(
    layout: &Layout,
    terminal: &Path,
    batch: u64,
) -> Result<PathBuf, Error> {
    let legacy = PathBuf::from(format!("batch-{batch}/finished.json"));
    if terminal == legacy {
        return Ok(layout.root.join(format!("batch-{batch}")));
    }
    let parent = terminal
        .parent()
        .ok_or_else(|| recovery("outcome-free batch terminal has no parent"))?;
    if parent.file_name() != Some(std::ffi::OsStr::new(&format!("batch-{batch}")))
        || !parent.starts_with(".asb-generations")
    {
        return Err(recovery(
            "outcome-free batch terminal is outside an owned batch root",
        ));
    }
    Ok(layout.root.join(parent))
}

fn archive_generation_roots(layout: &Layout, archive: &Path) -> Result<(), Error> {
    let generations = layout.root.join(".asb-generations");
    if !real_directory(&generations)? {
        return Ok(());
    }
    for entry in fs::read_dir(&generations).map_err(io_error)? {
        let source = entry.map_err(io_error)?.path();
        if !source.is_dir() {
            continue;
        }
        let name = source
            .file_name()
            .ok_or_else(|| recovery("outcome-free generation root has no name"))?;
        let target = archive.join("historical/generations").join(name);
        fs::create_dir_all(
            target
                .parent()
                .ok_or_else(|| recovery("historical generation evidence has no parent"))?,
        )
        .map_err(io_error)?;
        fs::rename(source, target).map_err(io_error)?;
    }
    Ok(())
}

fn eligible(
    layout: &Layout,
    configuration: usize,
    batch: u64,
    generation: Option<&str>,
    inventory: &Inventory,
) -> Result<PathBuf, Error> {
    let slots = layout
        .plan
        .slots
        .iter()
        .filter(|slot| slot.batch == batch)
        .collect::<Vec<_>>();
    if slots.is_empty() {
        return Err(recovery("outcome-free batch has no planned slots"));
    }
    for slot in slots {
        let key = (configuration, slot.id);
        if layout.outcome(slot)?.is_some()
            || inventory.actual.contains_key(&key)
            || inventory.unsettled.contains(&key)
        {
            return Err(recovery(
                "outcome-free batch recovery requires no completed or unsettled slot",
            ));
        }
    }
    let terminal = terminal_record_relative(layout, batch, generation)?;
    let finished: Value = serde_json::from_slice(&reading::bounded_relative(
        &layout.root,
        &terminal,
        64 * 1024,
    )?)
    .map_err(|_| recovery("outcome-free batch terminal record is malformed"))?;
    if finished["schema_version"] != 1
        || finished["failed"] != true
        || finished["stopped"] != true
        || finished["completed_executions"] != 0
    {
        return Err(recovery(
            "outcome-free batch recovery requires a stopped failed batch with zero completed executions",
        ));
    }
    let root = batch_root_from_terminal(layout, &terminal, batch)?;
    if !real_directory(&root)? {
        return Err(recovery("outcome-free batch root is not a real directory"));
    }
    Ok(terminal)
}

impl Inputs {
    pub(in crate::sweep) fn recover_outcome_free_batch_inner(
        &self,
        parent: &Path,
        results: &Path,
        plan: &SweepPlan,
    ) -> Result<Value, Error> {
        let owned = self.own_campaigns(parent, results, plan)?;
        let generation = self
            .read_generation_catalog(results, plan, &owned.layouts)?
            .map(|catalog| {
                catalog
                    .head_snapshot()
                    .map(|snapshot| snapshot.admission.generation)
            })
            .transpose()?;
        let mut pending = Vec::new();
        for (configuration, layout) in owned.layouts.iter().enumerate() {
            for batch in 1..=layout.plan.repeat {
                if let Some(receipt) =
                    read_receipt_value(&self.directory, configuration, batch, &self.initial.digest)?
                    && receipt.status == "pending"
                {
                    pending.push((configuration, layout, batch, receipt));
                }
            }
        }
        if pending.len() > 1 {
            return Err(recovery(
                "multiple outcome-free batch recoveries are incomplete",
            ));
        }
        if let Some((configuration, layout, batch, mut receipt)) = pending.pop() {
            let sources = batch_roots(layout, batch)?;
            let archive = archive_root_from_receipt(layout, &receipt.archive)?;
            match (sources.as_slice(), real_directory(&archive)?) {
                ([_source], false) => {
                    fs::remove_file(receipt_path(&self.directory, configuration, batch))
                        .map_err(io_error)?;
                }
                ([], true) => {
                    let fingerprint = self.snapshotter.fingerprint(
                        &[Root::path("outcome-free-batch-archive", archive)],
                        &context(),
                        false,
                        &[],
                    )?;
                    receipt.status = "complete".into();
                    receipt.fingerprint = Some(fingerprint);
                    write_receipt(
                        &receipt_path(&self.directory, configuration, batch),
                        &receipt,
                        true,
                    )?;
                    self.seal_campaigns(parent, results, plan)?;
                    return Ok(serde_json::json!({
                        "type":"outcome-free-batch-recovery",
                        "configuration":configuration,
                        "batch":batch,
                        "archive":archive_relative(batch),
                        "agent_executions":0,
                        "evaluations":0,
                    }));
                }
                _ => {
                    return Err(recovery(
                        "outcome-free batch recovery pending state has unsafe source/archive roots",
                    ));
                }
            }
        }
        let inventory = Inventory::read(
            &owned.layouts,
            &self.directory,
            &self.initial.digest,
            &self.snapshotter,
        )?;
        let mut candidate = None;
        for (configuration, layout) in owned.layouts.iter().enumerate() {
            for batch in 1..=layout.plan.repeat {
                if read_receipt(
                    &self.directory,
                    layout,
                    configuration,
                    batch,
                    &self.initial.digest,
                    &self.snapshotter,
                )? {
                    continue;
                }
                let terminal = match eligible(
                    layout,
                    configuration,
                    batch,
                    generation.as_deref(),
                    &inventory,
                ) {
                    Ok(terminal) => terminal,
                    Err(Error::Input(_)) => continue,
                    Err(error) => return Err(error),
                };
                if candidate
                    .replace((configuration, layout, batch, terminal))
                    .is_some()
                {
                    return Err(recovery(
                        "outcome-free batch recovery requires exactly one eligible batch",
                    ));
                }
            }
        }
        let Some((configuration, layout, batch, terminal)) = candidate else {
            return Err(recovery("no eligible outcome-free failed batch was found"));
        };
        let path = receipt_path(&self.directory, configuration, batch);
        let archive_relative = archive_relative_next(layout, batch);
        let archive = layout.root.join(&archive_relative);
        if archive.exists() {
            return Err(recovery("outcome-free batch archive already exists"));
        }
        if read_receipt_value(&self.directory, configuration, batch, &self.initial.digest)?
            .is_some()
        {
            let history =
                receipt_history_path(&self.directory, configuration, batch, &archive_relative)?;
            if !history.exists() {
                fs::create_dir_all(
                    history
                        .parent()
                        .ok_or_else(|| recovery("receipt history has no parent"))?,
                )
                .map_err(io_error)?;
                fs::copy(&path, &history).map_err(io_error)?;
            }
        }
        let mut receipt = Receipt {
            schema_version: 1,
            scope: SCOPE.into(),
            status: "pending".into(),
            configuration,
            batch,
            input_digest: self.initial.digest.clone(),
            archive: archive_relative.clone(),
            fingerprint: None,
        };
        write_receipt(&path, &receipt, true)?;
        let batch_root = batch_root_from_terminal(layout, &terminal, batch)?;
        fs::create_dir_all(
            archive
                .parent()
                .ok_or_else(|| recovery("outcome-free batch archive has no parent"))?,
        )
        .map_err(io_error)?;
        fs::rename(&batch_root, &archive).map_err(|error| {
            Error::Input(format!(
                "cannot atomically archive outcome-free batch evidence {}: {error}; preserve the original campaign and use one filesystem for campaign data",
                batch_root.display()
            ))
        })?;
        // Failed retries can leave older generated batch roots behind. Keep
        // them as evidence, but move them out of the runtime generation tree
        // so continuation does not mistake them for live completion input.
        for source in batch_roots(layout, batch)? {
            if source == archive || !source.exists() {
                continue;
            }
            let relative = source
                .strip_prefix(&layout.root)
                .map_err(|_| recovery("outcome-free batch source escaped campaign root"))?;
            let preserved = archive.join("historical").join(relative);
            fs::create_dir_all(
                preserved
                    .parent()
                    .ok_or_else(|| recovery("historical batch evidence has no parent"))?,
            )
            .map_err(io_error)?;
            fs::rename(source, preserved).map_err(io_error)?;
        }
        archive_generation_roots(layout, &archive)?;
        let fingerprint = self.snapshotter.fingerprint(
            &[Root::path("outcome-free-batch-archive", archive)],
            &context(),
            false,
            &[],
        )?;
        receipt.status = "complete".into();
        receipt.fingerprint = Some(fingerprint);
        write_receipt(&path, &receipt, true)?;
        self.seal_campaigns(parent, results, plan)?;
        let legacy_terminal = format!("batch-{batch}/finished.json");
        let terminal_record = if terminal == Path::new(&legacy_terminal) {
            archive_relative.join("finished.json")
        } else {
            terminal
        };
        Ok(serde_json::json!({
            "type":"outcome-free-batch-recovery",
            "configuration":configuration,
            "batch":batch,
            "terminal_record":terminal_record,
            "agent_executions":0,
            "evaluations":0,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sweep::checkpoint::campaigns::tests::settlement_tests;

    fn make_outcome_free(layout: &Layout, keep_first_completion: bool) {
        let batch = layout.root.join("batch-1");
        for directory in [batch.join("outcomes"), batch.join(".asb-completions")] {
            let mut entries = fs::read_dir(&directory)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            entries.sort();
            for (index, path) in entries.into_iter().enumerate() {
                if keep_first_completion && index == 0 {
                    continue;
                }
                fs::remove_file(path).unwrap();
            }
        }
        fs::write(
            batch.join("finished.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "completed_executions": 0,
                "failed": true,
                "stopped": true,
                "failure_reason": "collector startup failed before agent execution"
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn outcome_free_batch_recovery_archives_every_failed_batch_artifact() {
        let (fixture, plan, results, inputs) =
            settlement_tests::fixture_concurrency(crate::tape::Mode::Replay, false, false, 2);
        let layout = plan.read_campaigns(&results).unwrap().remove(0);
        make_outcome_free(&layout, false);

        let report = inputs
            .recover_outcome_free_batch_inner(&fixture.0, &results, &plan)
            .unwrap();
        assert_eq!(report["configuration"], 0);
        assert_eq!(report["batch"], 1);
        assert!(!layout.root.join("batch-1").exists());
        assert!(
            layout
                .root
                .join(archive_relative(1))
                .join("finished.json")
                .is_file()
        );
        assert!(
            layout
                .root
                .join(report["terminal_record"].as_str().unwrap())
                .is_file()
        );
        assert!(
            read_receipt(
                &inputs.directory,
                &layout,
                0,
                1,
                &inputs.initial.digest,
                &inputs.snapshotter,
            )
            .unwrap()
        );
        let inventory = Inventory::read(
            &[layout],
            &inputs.directory,
            &inputs.initial.digest,
            &inputs.snapshotter,
        )
        .unwrap();
        for slot in inventory
            .actual
            .keys()
            .filter(|(configuration, _)| *configuration == 0)
        {
            assert_ne!(*slot, (0, 1));
            assert_ne!(*slot, (0, 2));
        }
        assert!(!inventory.observed.contains(&(0, 1)));
        assert!(!inventory.observed.contains(&(0, 2)));
    }

    #[test]
    fn outcome_free_batch_recovery_rejects_any_completed_slot() {
        let (fixture, plan, results, inputs) =
            settlement_tests::fixture_concurrency(crate::tape::Mode::Replay, false, false, 2);
        let layout = plan.read_campaigns(&results).unwrap().remove(0);
        make_outcome_free(&layout, true);

        let error = inputs
            .recover_outcome_free_batch_inner(&fixture.0, &results, &plan)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no eligible outcome-free failed batch")
        );
        assert!(layout.root.join("batch-1").is_dir());
    }

    #[test]
    fn repeated_outcome_free_recovery_uses_a_new_archive_path() {
        let (fixture, plan, results, inputs) =
            settlement_tests::fixture_concurrency(crate::tape::Mode::Replay, false, false, 1);
        let layout = plan.read_campaigns(&results).unwrap().remove(0);
        assert_eq!(archive_relative_next(&layout, 1), archive_relative(1));
        fs::create_dir_all(layout.root.join(archive_relative(1))).unwrap();
        assert_eq!(
            archive_relative_next(&layout, 1),
            PathBuf::from(".asb-recovery/outcome-free-batch/000001-000002")
        );
        fs::create_dir_all(
            layout
                .root
                .join(".asb-recovery/outcome-free-batch/000001-000002"),
        )
        .unwrap();
        assert_eq!(
            archive_relative_next(&layout, 1),
            PathBuf::from(".asb-recovery/outcome-free-batch/000001-000003")
        );
        drop(fixture);
        drop(inputs);
    }

    #[test]
    fn generated_batch_roots_are_included_in_recovery_inventory() {
        let (fixture, plan, results, inputs) =
            settlement_tests::fixture_concurrency(crate::tape::Mode::Replay, false, false, 1);
        let layout = plan.read_campaigns(&results).unwrap().remove(0);
        let generated = layout.root.join(".asb-generations/test-generation/batch-1");
        fs::create_dir_all(&generated).unwrap();
        let roots = batch_roots(&layout, 1).unwrap();
        assert_eq!(roots.len(), 2);
        assert!(roots.contains(&generated));
        drop(fixture);
        drop(inputs);
    }

    #[test]
    fn generated_terminal_resolves_to_its_generated_batch_root() {
        let (fixture, plan, results, inputs) =
            settlement_tests::fixture_concurrency(crate::tape::Mode::Replay, false, false, 1);
        let layout = plan.read_campaigns(&results).unwrap().remove(0);
        let terminal = Path::new(".asb-generations/generation-a/batch-1/finished.json");
        let expected = layout.root.join(".asb-generations/generation-a/batch-1");
        fs::create_dir_all(&expected).unwrap();
        assert_eq!(
            batch_root_from_terminal(&layout, terminal, 1).unwrap(),
            expected
        );
        drop(fixture);
        drop(inputs);
    }

    #[test]
    fn pending_recovery_before_the_archive_rename_restarts_the_transaction() {
        let (fixture, plan, results, inputs) =
            settlement_tests::fixture_concurrency(crate::tape::Mode::Replay, false, false, 1);
        let layout = plan.read_campaigns(&results).unwrap().remove(0);
        make_outcome_free(&layout, false);
        let path = receipt_path(&inputs.directory, 0, 1);
        write_receipt(
            &path,
            &Receipt {
                schema_version: 1,
                scope: SCOPE.into(),
                status: "pending".into(),
                configuration: 0,
                batch: 1,
                input_digest: inputs.initial.digest.clone(),
                archive: archive_relative(1),
                fingerprint: None,
            },
            false,
        )
        .unwrap();

        inputs
            .recover_outcome_free_batch_inner(&fixture.0, &results, &plan)
            .unwrap();
        assert!(!layout.root.join("batch-1").exists());
        assert!(
            read_receipt(
                &inputs.directory,
                &layout,
                0,
                1,
                &inputs.initial.digest,
                &inputs.snapshotter,
            )
            .unwrap()
        );
    }

    #[test]
    fn pending_recovery_after_the_archive_rename_completes_without_execution() {
        let (fixture, plan, results, inputs) =
            settlement_tests::fixture_concurrency(crate::tape::Mode::Replay, false, false, 1);
        let layout = plan.read_campaigns(&results).unwrap().remove(0);
        make_outcome_free(&layout, false);
        let archive = layout.root.join(archive_relative(1));
        fs::create_dir_all(archive.parent().unwrap()).unwrap();
        fs::rename(layout.root.join("batch-1"), &archive).unwrap();
        write_receipt(
            &receipt_path(&inputs.directory, 0, 1),
            &Receipt {
                schema_version: 1,
                scope: SCOPE.into(),
                status: "pending".into(),
                configuration: 0,
                batch: 1,
                input_digest: inputs.initial.digest.clone(),
                archive: archive_relative(1),
                fingerprint: None,
            },
            false,
        )
        .unwrap();

        let report = inputs
            .recover_outcome_free_batch_inner(&fixture.0, &results, &plan)
            .unwrap();
        assert_eq!(report["agent_executions"], 0);
        assert_eq!(report["archive"].as_str(), archive_relative(1).to_str());
        assert!(
            read_receipt(
                &inputs.directory,
                &layout,
                0,
                1,
                &inputs.initial.digest,
                &inputs.snapshotter,
            )
            .unwrap()
        );
    }
}
