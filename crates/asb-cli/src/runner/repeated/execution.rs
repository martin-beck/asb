//! Shared actual batch execution for fresh campaigns and admitted continuations.
use super::*;
use crate::runner::campaign::generation::{Batch, StartReference};
use std::collections::BTreeMap;

pub(super) struct BatchRun<'a> {
    pub config_path: &'a Path,
    pub plan: &'a Plan,
    pub root: &'a Path,
    pub options: &'a ActivationOptions,
    pub identity: BatchIdentity,
    pub batch_root: &'a Path,
    pub material_path: PathBuf,
    pub finished_path: PathBuf,
    pub generation: Option<StartReference>,
    pub cassette: Option<&'a str>,
    /// Recorder-owned provider producer, retained independently from a
    /// user-facing cassette spelling.  Adapter collection receives only its
    /// bounded opaque bytes after the proxy settles.
    pub provider_capture_path: Option<PathBuf>,
    pub recording: Option<Arc<super::super::recording::Recorder>>,
    pub reused: Option<&'a BTreeMap<usize, serde_json::Value>>,
}

pub(super) struct SettledBatch {
    pub prepared: Option<Compatibility>,
    pub failed: bool,
    pub stopped: bool,
    pub reason: Option<String>,
    pub quiescent: bool,
    pub completed: usize,
}

impl BatchRun<'_> {
    pub(super) fn execute(
        self,
        admission: batch::Admission,
        output: &mut dyn Write,
        warnings: &Mutex<BTreeSet<String>>,
    ) -> Result<SettledBatch, Error> {
        let number = self.identity.number;
        let observer = Observer {
            root: self.root.to_path_buf(),
            directory: self.batch_root.to_path_buf(),
            identity: self.identity.clone(),
            generation: self.generation,
            completed: Mutex::new(BTreeSet::new()),
            cleanup_confirmed: AtomicBool::new(true),
        };
        let material_path = self.material_path;
        let recorder = self.recording.clone();
        let members = admission.cohort().admitted_slots.clone();
        let dispatch = admission.observe(Arc::new(move |compatibility| {
            publish(&material_path, &json!({"schema_version":1,"scope":"verified-prepared-material","compatibility":compatibility}))?;
            if let Some(recorder) = &recorder { recorder.prepare(compatibility, number, &members)?; }
            Ok(())
        }));
        let mut selected = self.plan.configuration.clone();
        selected
            .planning
            .get_or_insert_with(Default::default)
            .repetition = crate::planning::Repetition {
            index: number as usize,
            total: self.plan.repeat as usize,
        };
        let mut policy =
            crate::planning::resolve_for_run(&selected, self.options.measurement_override)?;
        if let Some(saved) = &self.options.policy {
            policy.task_requirements = saved.task_requirements.clone();
        }
        selected.workspace = path_text(&self.batch_root.join("workspace"))?;
        selected.log_file = path_text(&self.batch_root.join("run.jsonl"))?;
        // Campaign preparation owns its private input material. In particular,
        // a replay bundle binds the material identity recorded before this
        // invocation: copying the caller's mutable workspace here would make a
        // later source-tree change alter that identity and invalidate a valid
        // offline replay. Direct runs seed their fresh workspace separately.
        let measures = self
            .plan
            .measurement_root
            .as_ref()
            .map(|root| root.join(format!("batch-{number}/measurements")));
        if let Some(measurements) = selected.measurements.as_mut() {
            measurements.directory = path_text(
                measures
                    .as_deref()
                    .unwrap_or(&self.batch_root.join("measurements")),
            )?;
        }
        let batch_log_file = selected.log_file.clone();
        let measurement_override = self
            .options
            .measurement_override
            .then(|| path_text(measures.as_deref().expect("explicit override is measured")))
            .transpose()?;
        let mode = match self.plan.mode {
            crate::planning::ExecutionMode::Live => None,
            crate::planning::ExecutionMode::Record => Some(crate::tape::Mode::Record),
            crate::planning::ExecutionMode::Replay => Some(crate::tape::Mode::Replay),
            crate::planning::ExecutionMode::Unknown => {
                return Err(Error::Input(
                    "unknown campaign execution mode cannot be activated".into(),
                ));
            }
        };
        // Bind each admitted replica to its already selected replay input.
        // This is execution-private and never reconstructed from an adjacent
        // provider cassette name.
        let native_tool_inputs: Vec<_> = if mode == Some(crate::tape::Mode::Replay) {
            let start = (number as usize - 1) * self.plan.concurrency;
            self.plan.slots[start..start + self.plan.concurrency]
                .iter()
                .map(|slot| {
                    &self.plan.replay_inputs[slot.replay_input.expect("planned replay input")]
                })
                .collect()
        } else {
            Vec::new()
        };
        let result = super::super::run_admitted_batch(
            self.config_path,
            selected,
            RunOptions {
                invocation_dir: self.options.invocation_dir.as_deref(),
                json: self.options.json,
                show_output: self.options.show_output,
                mode,
                tool_mode: self.options.tool_mode,
                tool_mode_explicit: self.options.tool_mode_explicit,
                mcp_mode: self.options.mcp_mode,
                native_capsule_record: self.options.native_capsule_record.as_deref(),
                native_capsule_replay: self.options.native_capsule_replay.as_deref(),
                respect_record_timings: self.plan.respect_record_timings,
                policy: Some(&policy),
                requirements_prepared: self.options.requirements_prepared,
                batch: Some(&self.identity),
                native_tool_inputs: (!native_tool_inputs.is_empty())
                    .then_some(native_tool_inputs.as_slice()),
                measurement_dir: measurement_override.as_deref(),
                cassette: self.cassette,
                recording: self.recording.as_deref(),
                provider_capture_path: self.provider_capture_path.as_deref(),
                on_completion: Some(&observer),
                reused: self.reused,
                ..Default::default()
            },
            &dispatch,
            output,
            warnings,
        );
        publish_configured_logs(
            self.config_path,
            &self.plan.configuration.log_file,
            &batch_log_file,
            self.plan.concurrency,
        )?;
        let prepared = dispatch.material().confirmed_identity();
        let completed = observer
            .completed
            .lock()
            .map_err(|_| Error::Run("execution completion state was interrupted".into()))?
            .len();
        let cleanup = observer.cleanup_confirmed.load(Ordering::Acquire);
        let stopped = must_stop(
            &result,
            completed,
            dispatch.count(),
            prepared.is_ok(),
            cleanup,
        );
        let reason = result
            .as_ref()
            .err()
            .map(|error| error.to_string().chars().take(4096).collect::<String>())
            .or_else(|| {
                stopped.then(|| {
                    "execution evidence or cleanup was not confirmed; inspect retained outcomes"
                        .into()
                })
            });
        let failed = result.is_err();
        publish(
            &self.finished_path,
            &json!({"schema_version":1,"completed_executions":completed,"failed":failed||stopped,"stopped":stopped,"failure_reason":reason}),
        )?;
        Ok(SettledBatch {
            prepared: prepared.ok(),
            failed,
            stopped,
            reason,
            quiescent: completed == dispatch.count() && cleanup,
            completed,
        })
    }
}

/// Campaign records stay under the private batch directory for continuation,
/// while the configured log remains the stable user-facing result location.
/// Copy only completed batch records, append rather than replace an existing
/// configured evidence log, and never treat an absent incomplete record as a
/// successful outcome.
fn publish_configured_logs(
    config_path: &Path,
    configured: &str,
    batch: &str,
    concurrency: usize,
) -> Result<(), Error> {
    use std::io::Write;

    for index in 1..=concurrency {
        let source = if concurrency == 1 {
            PathBuf::from(batch)
        } else {
            PathBuf::from(super::super::parallel_path(batch, index))
        };
        let destination = if concurrency == 1 {
            super::super::resolve_log_path(config_path, configured)
        } else {
            super::super::resolve_log_path(
                config_path,
                &super::super::parallel_path(configured, index),
            )
        };
        let bytes = match std::fs::read(&source) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Error::Io(error.to_string())),
        };
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| Error::Io(error.to_string()))?;
        }
        let mut output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&destination)
            .map_err(|error| Error::Io(error.to_string()))?;
        output
            .write_all(&bytes)
            .and_then(|()| output.sync_all())
            .map_err(|error| Error::Io(error.to_string()))?;
    }
    Ok(())
}

/// Consumed only from the checkpoint's non-deserializable admitted capability.
/// The parent owns all original campaign locks through execution and reporting.
pub(crate) struct Continuation {
    pub invocation: PathBuf,
    pub root: PathBuf,
    pub plan: Plan,
    pub completed: BTreeMap<u64, serde_json::Value>,
    pub pending: Vec<Batch>,
    pub compatibility: Option<Compatibility>,
    pub reference: StartReference,
    pub recording: Option<Arc<super::super::recording::Recorder>>,
}

pub(crate) fn continue_campaign(
    continued: Continuation,
    json: bool,
    show_output: bool,
    invocation_dir: Option<&Path>,
    policy: Option<&crate::planning::ResolvedPlan>,
    output: &mut dyn Write,
    warnings: &Mutex<BTreeSet<String>>,
) -> Result<(), Error> {
    let Continuation {
        invocation,
        root,
        plan,
        completed,
        pending,
        mut compatibility,
        reference,
        recording,
    } = continued;
    plan.validate_layout()?;
    // A generation covers the whole sweep, including configurations already
    // complete in an ancestor. Those configurations have no new publication
    // owner and must not create generation directories or replace their ledger.
    if pending.is_empty() {
        if recording.is_some()
            || completed.len() != plan.slots.len()
            || plan
                .slots
                .iter()
                .any(|slot| !completed.contains_key(&slot.id))
        {
            return Err(Error::Input(
                "completed-only continuation has inconsistent slot or writer ownership".into(),
            ));
        }
        return super::super::reuse::render_reused(
            &completed.into_values().collect::<Vec<_>>(),
            json,
            output,
        );
    }
    // Recording unions need a distinct publication owner. Never accidentally
    // reopen or replace a finalized original recording with Recorder::new.
    if (plan.mode == crate::planning::ExecutionMode::Record) != recording.is_some() {
        return Err(Error::Input(
            "recording continuation lost its owned union writer".into(),
        ));
    }
    let options = ActivationOptions {
        invocation_dir: invocation_dir.map(Path::to_path_buf),
        json,
        show_output,
        quiet_output: false,
        requirements_prepared: false,
        policy: policy.cloned(),
        measurement_override: plan.measurement_storage
            == Some(super::super::campaign::MeasurementStorage::Override),
        tool_mode: None,
        tool_mode_explicit: false,
        mcp_mode: None,
        native_capsule_record: None,
        native_capsule_replay: None,
    };
    let pending_inputs: BTreeSet<_> = pending
        .iter()
        .flat_map(|batch| {
            let start = ((batch.batch - 1) * plan.concurrency as u64) as usize;
            plan.slots[start..start + plan.concurrency]
                .iter()
                .filter(|slot| batch.cohort.admitted_slots.contains(&slot.replica))
                .filter_map(|slot| slot.replay_input)
        })
        .collect();
    replay::disclose_incomplete(
        plan.replay_inputs
            .iter()
            .enumerate()
            .filter_map(|(index, input)| pending_inputs.contains(&index).then_some(input)),
        json,
        output,
    )?;
    let generation_root = root.join(".asb-generations").join(&reference.generation);
    private_directory(&root.join(".asb-generations"), true)?;
    private_directory(&generation_root, recording.is_some())?;
    let mut failures = 0usize;
    let mut stopped = false;
    let mut producers_quiescent = true;
    let mut batches = pending.into_iter().peekable();
    for number in 1..=plan.repeat {
        let start = ((number - 1) * plan.concurrency as u64) as usize;
        let slots = &plan.slots[start..start + plan.concurrency];
        let reused: BTreeMap<_, _> = slots
            .iter()
            .filter_map(|slot| {
                completed
                    .get(&slot.id)
                    .map(|summary| (slot.replica, summary.clone()))
            })
            .collect();
        if batches.peek().is_none_or(|batch| batch.batch != number) {
            let result = super::super::reuse::render_reused(
                &reused.into_values().collect::<Vec<_>>(),
                json,
                output,
            );
            if matches!(result, Err(Error::Io(_))) {
                return result;
            }
            failures += usize::from(result.is_err());
            continue;
        }
        let batch = batches.next().expect("peeked batch");
        let dispatch = batch::Admission::from_cohort(
            batch.cohort.clone(),
            compatibility
                .clone()
                .or_else(|| plan.replay_compatibility.clone()),
            plan.admission,
        )?;
        let batch_root = root.join(format!("batch-{number}"));
        private_directory(&batch_root, true)?;
        for name in ["starts", "outcomes", "allocations"] {
            private_directory(&batch_root.join(name), true)?;
        }
        let generation_batch = root.join(batch.directory(&reference.generation));
        private_directory(&generation_batch, false)?;
        let cassette = if recording.is_some() {
            Some(path_text(&super::super::recording::base(&root, number))?)
        } else {
            (plan.mode == crate::planning::ExecutionMode::Replay)
                .then(|| {
                    replay::stage_selected(
                        &root,
                        &plan,
                        number,
                        &batch.cohort.admitted_slots,
                        &generation_batch,
                    )
                })
                .transpose()?
        };
        let identity = BatchIdentity {
            replay: replay::saved_batch_choices(&plan, number),
            campaign: plan.campaign.clone(),
            number,
            repeat: plan.repeat,
            concurrency: plan.concurrency,
        };
        let settled = BatchRun {
            config_path: &invocation,
            plan: &plan,
            root: &root,
            options: &options,
            identity,
            batch_root: &batch_root,
            material_path: generation_batch.join("prepared.json"),
            finished_path: generation_batch.join("finished.json"),
            generation: Some(reference.clone()),
            cassette: cassette.as_deref(),
            provider_capture_path: recording
                .as_ref()
                .map(|_| super::super::recording::producer(&root, number, 1, plan.concurrency)),
            recording: recording.clone(),
            reused: Some(&reused),
        }
        .execute(dispatch, output, warnings)?;
        failures += usize::from(settled.failed || settled.stopped);
        producers_quiescent &= settled.quiescent;
        stopped = settled.stopped;
        if stopped {
            break;
        }
        compatibility = settled.prepared;
    }
    if let Some(recorder) = &recording
        && let Some(publication) = recorder.finalize(producers_quiescent)?
    {
        recorder.publish_union(&publication)?;
        warnings
            .lock()
            .map_err(|_| Error::Run("recording warnings were interrupted".into()))?
            .extend(publication.warnings);
    }
    publish(
        &generation_root.join("finished.json"),
        &json!({"schema_version":1,"failed_batches":failures,"stopped":stopped}),
    )?;
    if failures > 0 || stopped {
        Err(Error::Run(format!(
            "continued campaign has {failures} failed batch(es); preserved outcomes remain unchanged"
        )))
    } else {
        Ok(())
    }
}
