//! Fresh sequential batches with a durable intended-slot plan and outcome links.
mod execution;
mod legacy;
mod replay;
pub(crate) use super::campaign::ExecutionIdentity;
use super::campaign::{MAX_EXECUTIONS, Plan, ReplayInput, Slot, planned_slots};
use super::{RunOptions, batch, replica};
use crate::{Error, benchmark::BenchmarkConfig, cassette::Compatibility, io_error};
pub(crate) use execution::{Continuation, continue_campaign};
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const MAX_PLAN_BYTES: usize = 16 * 1024 * 1024;
const MAX_OUTCOME_BYTES: u64 = 1024 * 1024;

#[cfg(test)]
pub(crate) fn take_legacy_observation_count() -> usize {
    legacy::take_transfer_count()
}

#[derive(Clone, Serialize)]
pub(crate) struct BatchIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    replay: Option<replay::BatchChoices>,
    campaign: String,
    number: u64,
    repeat: u64,
    concurrency: usize,
}

impl BatchIdentity {
    pub(super) fn number(&self) -> u64 {
        self.number
    }
    pub(crate) fn admission(&self) -> crate::planning::BatchAdmissionPolicy {
        crate::planning::BatchAdmissionPolicy::current()
    }
    pub(crate) fn execution(&self, replica: replica::Identity) -> ExecutionIdentity {
        ExecutionIdentity {
            replay_track: self
                .replay
                .as_ref()
                .map(|choices| choices.tracks[replica.index - 1]),
            replay_seed: self.replay.as_ref().and_then(|choices| choices.seed),
            replay_algorithm: self
                .replay
                .as_ref()
                .and_then(|choices| choices.algorithm.clone()),
            admission: self.admission(),
            campaign: self.campaign.clone(),
            batch: self.number,
            replica: replica.index,
            slot: (self.number - 1) * self.concurrency as u64 + replica.index as u64,
            total: self.repeat * self.concurrency as u64,
        }
    }
    pub(super) fn replay_statistics(&self, replica: usize) -> Option<crate::cassette::Statistics> {
        self.replay
            .as_ref()
            .and_then(|replay| replay.statistics.get(replica.checked_sub(1)?)?.clone())
    }
}

pub(super) fn run(
    config_path: &Path,
    config: BenchmarkConfig,
    repeat: u64,
    options: RunOptions<'_>,
    output: &mut dyn Write,
    warnings: &Mutex<BTreeSet<String>>,
) -> Result<(), Error> {
    activate(
        Prepared::prepare(config_path, config, repeat, options, None)?,
        output,
        warnings,
    )
}

/// Own frozen planning facts and opened sources, never an active process/lock.
pub(crate) struct Prepared {
    config_path: PathBuf,
    options: ActivationOptions,
    parent: PathBuf,
    root: PathBuf,
    plan: Plan,
    bytes: Vec<u8>,
    replay: Option<PreparedReplay>,
}

/// Metadata-only planning owner. Its plan is deliberately not publishable until
/// the consuming observation step supplies genuine legacy input digests.
pub(crate) struct Inspection {
    config_path: PathBuf,
    options: ActivationOptions,
    parent: PathBuf,
    root: PathBuf,
    plan: Plan,
    replay: Option<InspectedReplay>,
}

enum InspectedReplay {
    Selected(Box<replay::Inspection>),
    Legacy(legacy::Inspection),
}

#[derive(Clone, Copy, Default)]
pub(crate) struct InputCost {
    pub inputs: usize,
    pub bytes: u64,
    pub observed_bytes: u64,
    pub files: usize,
    pub directory_bytes: u64,
    pub manifest_bytes: u64,
}

impl InputCost {
    pub(crate) fn checked_add(self, next: Self) -> Result<Self, Error> {
        let invalid =
            |reason: &str| Error::Input(format!("sweep replay input admission: {reason}"));
        Ok(Self {
            inputs: self
                .inputs
                .checked_add(next.inputs)
                .filter(|value| *value <= MAX_EXECUTIONS as usize)
                .ok_or_else(|| invalid("selected inputs exceed execution population"))?,
            bytes: self
                .bytes
                .checked_add(next.bytes)
                .filter(|value| *value <= 2 * 1024 * 1024 * 1024)
                .ok_or_else(|| invalid("private replay inputs exceed aggregate 2 GiB"))?,
            observed_bytes: self
                .observed_bytes
                .checked_add(next.observed_bytes)
                .filter(|value| *value <= 2 * 1024 * 1024 * 1024)
                .ok_or_else(|| invalid("legacy observations exceed aggregate 2 GiB"))?,
            files: self
                .files
                .checked_add(next.files)
                .filter(|value| *value <= 256)
                .ok_or_else(|| invalid("retained replay descriptors exceed 256"))?,
            directory_bytes: self
                .directory_bytes
                .checked_add(next.directory_bytes)
                .filter(|value| *value <= 64 * 1024 * 1024)
                .ok_or_else(|| invalid("archive directories exceed aggregate 64 MiB"))?,
            manifest_bytes: self
                .manifest_bytes
                .checked_add(next.manifest_bytes)
                .filter(|value| *value <= 256 * 1024 * 1024)
                .ok_or_else(|| invalid("archive manifests exceed aggregate 256 MiB"))?,
        })
    }
}

enum PreparedReplay {
    Selected(Box<replay::Prepared>),
    Legacy(legacy::Prepared),
}

/// Only presentation and saved policy survive preparation as execution options.
/// Routing/counts/timings come from the owned plan/source, not parallel copies.
struct ActivationOptions {
    invocation_dir: Option<PathBuf>,
    json: bool,
    show_output: bool,
    quiet_output: bool,
    requirements_prepared: bool,
    measurement_override: bool,
    policy: Option<crate::planning::ResolvedPlan>,
    tool_mode: Option<crate::tape::Mode>,
    tool_mode_explicit: bool,
    mcp_mode: Option<crate::tape::Mode>,
    native_capsule_record: Option<String>,
    native_capsule_replay: Option<String>,
}

impl Prepared {
    pub(crate) fn prepare(
        config_path: &Path,
        config: BenchmarkConfig,
        repeat: u64,
        options: RunOptions<'_>,
        campaign_parent: Option<&Path>,
    ) -> Result<Self, Error> {
        Inspection::prepare_selected(config_path, config, repeat, options, campaign_parent, None)?
            .observe()
    }
}

impl Inspection {
    pub(crate) fn validate_sources(&self) -> Result<(), Error> {
        match &self.replay {
            Some(InspectedReplay::Selected(selected)) => selected.validate(),
            Some(InspectedReplay::Legacy(legacy)) => legacy.validate(),
            None => Ok(()),
        }
    }

    pub(crate) fn prepare_selected(
        config_path: &Path,
        config: BenchmarkConfig,
        repeat: u64,
        options: RunOptions<'_>,
        campaign_parent: Option<&Path>,
        selection: Option<&crate::workloads::TaskSelection>,
    ) -> Result<Self, Error> {
        // Reject unsupported combinations and excessive populations before any IO
        // publication or dependency/environment preparation.
        repeat
            .checked_mul(config.concurrency as u64)
            .filter(|total| repeat > 0 && *total <= MAX_EXECUTIONS)
            .ok_or_else(|| {
                Error::Input(
                    "repeat must be positive and its concurrency product at most 65533".into(),
                )
            })?;
        // Repeated campaigns bypass the one-shot native command builder until
        // after their durable campaign plan has been prepared.  Validate this
        // capability here as well, so `--max-steps` can never be accepted and
        // then silently omitted for an adapter that has no client-enforced
        // turn limit.
        if options.max_steps.is_some()
            && crate::agents::find(&config.agent)
                .is_some_and(|adapter| !adapter.supports_max_steps())
        {
            return Err(Error::Input(format!(
                "agent `{}` does not support --max-steps; remove the flag or select an adapter with an enforced turn limit",
                config.agent
            )));
        }
        if options.on_completion.is_some()
            || options.replica.index != 1
            || options.replica.total != 1
            || options.reused.is_some()
            || options.preparation_gate.is_some()
            || options.launch_gate.is_some()
            || options.batch.is_some()
            || options.recording.is_some()
        {
            return Err(Error::Input(
                "repeated execution cannot reuse legacy observers, slots or preparation gates"
                    .into(),
            ));
        }
        crate::planning::resolve_snapshot(
            &config,
            options.measurement_dir.is_some(),
            options.policy,
        )?;
        if config
            .planning
            .as_ref()
            .is_some_and(|planning| planning.repetition != crate::planning::Repetition::default())
        {
            return Err(Error::Input("--repeat cannot combine with a nondefault planning.repetition; choose automated batches or manually identified configurations".into()));
        }
        let admission = crate::planning::BatchAdmissionPolicy::current();
        let (project_root, settings) = crate::project::load(config_path)?;
        let project_root = project_root.canonicalize().map_err(io_error)?;
        let campaign = crate::workloads::TaskIdentity::local("execution-campaign", None).attempt_id;
        let measured = options.measurement_dir.is_some()
            || config
                .measurements
                .as_ref()
                .is_some_and(|value| value.enabled);
        let mut slots = planned_slots(&config, repeat, measured)?;
        let replay_source = (options.mode == Some(crate::tape::Mode::Replay))
            .then(|| {
                super::replay_cassette(config_path, &config, options.cassette)
                    .and_then(|path| std::path::absolute(path).map_err(io_error))
            })
            .transpose()?;
        let replay = match replay::Inspection::open(config_path, &config, options, repeat)? {
            Some(selected) => Some(InspectedReplay::Selected(Box::new(selected))),
            None if options.mode == Some(crate::tape::Mode::Replay) => {
                Some(InspectedReplay::Legacy(legacy::Inspection::open(
                    config_path,
                    &config,
                    options,
                    &mut slots,
                )?))
            }
            None => None,
        };
        let parent = campaign_parent
            .map(|path| std::path::absolute(path).map_err(io_error))
            .transpose()?
            .unwrap_or_else(|| project_root.join("results/asb-executions"));
        let root = parent.join(&campaign);
        let recording_destination = (options.mode == Some(crate::tape::Mode::Record))
            .then(|| {
                let destination = options.cassette.map(PathBuf::from).unwrap_or_else(|| {
                    if campaign_parent.is_some() {
                        root.join("recording.asb.zip")
                    } else {
                        super::default_bundle_cassette(config_path, &config)
                    }
                });
                super::recording::inspect_destination(&destination)
            })
            .transpose()?;
        let measurement_root = measured
            .then(|| {
                options
                    .measurement_dir
                    .map(|path| std::path::absolute(path).map(|path| path.join(&campaign)))
                    .unwrap_or_else(|| Ok(root.clone()))
            })
            .transpose()
            .map_err(io_error)?;
        let recording_selection = if recording_destination.is_some() {
            Some(if let Some(selection) = selection {
                selection.clone()
            } else {
                let directory = crate::project::environment_path(
                    "ASB_WORKLOAD_DIR",
                    options.invocation_dir.unwrap_or_else(|| Path::new(".")),
                )
                .unwrap_or_else(|| crate::project::resolve(&project_root, &settings.workloads_dir));
                let catalog = crate::workloads::planning_catalog(&config.workload, &directory)
                    .map_err(Error::Input)?;
                crate::workloads::describe_selection(&config, &directory, catalog.as_ref())
                    .map_err(Error::Input)?
            })
        } else {
            None
        };
        let mut plan = Plan {
            replay_compatibility: replay.as_ref().and_then(|replay| match replay {
                InspectedReplay::Selected(selected) => selected.compatibility.clone(),
                InspectedReplay::Legacy(_) => None,
            }),
            recording_selection,
            recording_reservation: None,
            replay_source,
            measurement_storage: measured.then_some(if options.measurement_dir.is_some() {
                super::campaign::MeasurementStorage::Override
            } else {
                super::campaign::MeasurementStorage::Campaign
            }),
            replay_selection: replay.as_ref().and_then(|replay| match replay {
                InspectedReplay::Selected(selected) => selected.selection.clone(),
                InspectedReplay::Legacy(_) => None,
            }),
            recording: recording_destination.clone(),
            admission,
            schema_version: 1,
            scope: "intended-executions-not-attempt-evidence".into(),
            campaign: campaign.clone(),
            source: config_path.canonicalize().map_err(io_error)?,
            configuration_sha256: crate::cassette::digest(
                &serde_json::to_vec(&config).map_err(|error| Error::Input(error.to_string()))?,
            ),
            repeat,
            concurrency: config.concurrency,
            configuration: config,
            mode: match options.mode {
                Some(crate::tape::Mode::Record) => crate::planning::ExecutionMode::Record,
                Some(crate::tape::Mode::Replay) => crate::planning::ExecutionMode::Replay,
                _ => crate::planning::ExecutionMode::Live,
            },
            respect_record_timings: options.respect_record_timings,
            measurement_root: measurement_root.clone(),
            replay_inputs: Vec::new(),
            slots,
        };
        plan.recording_reservation = plan.recording_capacity()?;
        Ok(Self {
            // Anchor the invocation without following its final symlink: its
            // project authority can differ from the canonical source location.
            config_path: std::path::absolute(config_path).map_err(io_error)?,
            options: ActivationOptions {
                invocation_dir: options.invocation_dir.map(Path::to_path_buf),
                json: options.json,
                show_output: options.show_output,
                quiet_output: options.quiet_output,
                requirements_prepared: options.requirements_prepared,
                measurement_override: options.measurement_dir.is_some(),
                policy: options.policy.cloned(),
                tool_mode: options.tool_mode,
                tool_mode_explicit: options.tool_mode_explicit,
                mcp_mode: options.mcp_mode,
                native_capsule_record: options.native_capsule_record.map(Into::into),
                native_capsule_replay: options.native_capsule_replay.map(Into::into),
            },
            parent,
            root,
            plan,
            replay,
        })
    }

    pub(crate) fn input_cost(&self) -> Result<InputCost, Error> {
        match &self.replay {
            Some(InspectedReplay::Selected(selected)) => selected.input_cost(),
            Some(InspectedReplay::Legacy(legacy)) => Ok(legacy.input_cost()),
            None => Ok(InputCost::default()),
        }
    }

    pub(crate) fn recording_reservation(&self) -> Option<&crate::cassette::RecordingReservation> {
        self.plan.recording_reservation.as_ref()
    }

    pub(crate) fn observe(mut self) -> Result<Prepared, Error> {
        let replay = match self.replay {
            Some(InspectedReplay::Selected(selected)) => {
                let selected = selected.observe()?;
                self.plan.replay_inputs = selected.inputs(&mut self.plan.slots)?;
                Some(PreparedReplay::Selected(Box::new(selected)))
            }
            Some(InspectedReplay::Legacy(legacy)) => {
                let (legacy, inputs) = legacy.observe()?;
                self.plan.replay_inputs = inputs;
                Some(PreparedReplay::Legacy(legacy))
            }
            None => None,
        };
        self.plan.validate_layout()?;
        let bytes = encode(&self.plan)?;
        Ok(Prepared {
            config_path: self.config_path,
            options: self.options,
            parent: self.parent,
            root: self.root,
            plan: self.plan,
            bytes,
            replay,
        })
    }
}

impl Prepared {
    pub(crate) fn reference(&self, results: &Path) -> Result<super::campaign::Reference, Error> {
        let results = std::path::absolute(results).map_err(io_error)?;
        Ok(super::campaign::Reference {
            root: self
                .root
                .strip_prefix(results)
                .map_err(|_| {
                    Error::Input("campaign must be beneath its owning sweep results".into())
                })?
                .to_path_buf(),
            bytes: self.bytes.len() as u64,
            sha256: crate::cassette::digest(&self.bytes),
        })
    }

    /// Open source handles remain pinned; only their durable display locations
    /// change when the owning sweep preserves the previous results tree.
    pub(crate) fn preserve_source_locations(
        &mut self,
        old: &Path,
        preserved: &Path,
    ) -> Result<(), Error> {
        let old = std::path::absolute(old).map_err(io_error)?;
        let preserved = std::path::absolute(preserved).map_err(io_error)?;
        if let Some(source) = &mut self.plan.replay_source
            && let Ok(relative) = source.strip_prefix(&old)
        {
            *source = preserved.join(relative);
        }
        for input in &mut self.plan.replay_inputs {
            if let Ok(relative) = input.source.strip_prefix(&old) {
                input.source = preserved.join(relative);
            }
        }
        self.bytes = encode(&self.plan)?;
        Ok(())
    }
}

fn activate(
    prepared: Prepared,
    output: &mut dyn Write,
    warnings: &Mutex<BTreeSet<String>>,
) -> Result<(), Error> {
    execute(publish_campaign(prepared)?, output, warnings)
}

/// Published layout/input ownership is not evidence of an attempted execution.
pub(crate) struct PublishedCampaign {
    config_path: PathBuf,
    options: ActivationOptions,
    root: PathBuf,
    plan: Plan,
    private_cassette: Option<String>,
    replay: Option<Box<replay::Prepared>>,
    _owner: crate::output_lock::OutputLock,
}

pub(crate) fn publish_campaign(prepared: Prepared) -> Result<PublishedCampaign, Error> {
    let Prepared {
        config_path,
        options,
        parent,
        root,
        mut plan,
        bytes,
        replay,
    } = prepared;
    plan.validate_layout()?;
    // Inspection never reserved the output. Recheck before creating a campaign
    // or starting any dependency; the journal remains the final no-clobber owner.
    if let Some(destination) = &plan.recording {
        let observed = super::recording::inspect_destination(destination)?;
        if observed != *destination {
            return Err(Error::Input(
                "recording destination changed after planning".into(),
            ));
        }
    }
    private_directory(&parent, true)?;
    private_directory(&root, false)?;
    let _owner = crate::output_lock::OutputLock::acquire(
        &root.join(".owner.lock"),
        "execution campaign already has an owner",
    )
    .map_err(Error::Run)?;
    // Root-contained cassettes must not create/adopt the root ahead of its
    // exclusive owner. Inspection was not a reservation; recheck after owning it.
    if let Some(destination) = &plan.recording {
        let observed = super::recording::preflight(destination)?;
        if observed != *destination {
            return Err(Error::Input(
                "recording destination changed during publication".into(),
            ));
        }
    }
    publish_bytes(&root.join("plan.json"), &bytes)?;
    let (private_cassette, replay) = match replay {
        Some(PreparedReplay::Selected(mut selected)) => {
            selected.stage(&root, &mut plan.replay_inputs)?;
            (None, Some(selected))
        }
        Some(PreparedReplay::Legacy(legacy)) => (legacy.stage(&root, &plan)?, None),
        None => (None, None),
    };
    if let Some(measurement_root) = &plan.measurement_root
        && measurement_root != &root
    {
        let parent = measurement_root
            .parent()
            .ok_or_else(|| Error::Input("invalid measurement root".into()))?;
        private_directory(parent, true)?;
        private_directory(measurement_root, false)?;
    }
    Ok(PublishedCampaign {
        config_path,
        options,
        root,
        plan,
        private_cassette,
        replay,
        _owner,
    })
}

pub(crate) fn execute(
    published: PublishedCampaign,
    output: &mut dyn Write,
    warnings: &Mutex<BTreeSet<String>>,
) -> Result<(), Error> {
    let PublishedCampaign {
        config_path,
        options,
        root,
        plan,
        private_cassette,
        replay,
        _owner,
    } = published;
    let config = &plan.configuration;
    let repeat = plan.repeat;
    let total = plan.slots.len();
    let admission = plan.admission;
    let campaign = plan.campaign.clone();
    let recording = plan.recording.as_ref().map(|destination| {
        Arc::new(super::recording::Recorder::new(
            destination.clone(),
            root.clone(),
            campaign.clone(),
            repeat,
            config.concurrency,
            Some(&plan),
        ))
    });
    if options.json {
        serde_json::to_writer(
            &mut *output,
            &json!({"type":"execution-plan","path":root.join("plan.json"),"executions":total}),
        )
        .map_err(|error| Error::Io(error.to_string()))?;
        writeln!(output).map_err(io_error)?;
    } else if !options.quiet_output {
        writeln!(
            output,
            "Execution plan: {} ({total} executions)",
            root.join("plan.json").display()
        )
        .map_err(io_error)?;
    }
    if let Some(selection) = &plan.replay_selection {
        if options.json {
            serde_json::to_writer(
                &mut *output,
                &json!({"type":"replay-selection","selection":selection}),
            )
            .map_err(|error| Error::Io(error.to_string()))?;
            writeln!(output).map_err(io_error)?;
        } else if !options.quiet_output {
            writeln!(
                output,
                "Replay seed: {} ({})\nSelected track IDs: {}",
                selection.seed,
                selection.algorithm,
                selection
                    .tracks
                    .iter()
                    .map(u64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .map_err(io_error)?;
        }
    }
    replay::disclose_incomplete(&plan.replay_inputs, options.json, output)?;
    let mut reference: Option<Compatibility> = plan.replay_compatibility.clone();
    let mut failures = 0u64;
    let mut completed_executions = 0usize;
    let mut completed_batches = 0u64;
    let mut stopped = false;
    let mut first_reason = None;
    let mut producers_quiescent = true;
    for number in 1..=repeat {
        let identity = BatchIdentity {
            replay: replay
                .as_ref()
                .map(|replay| replay.batch_choices(&plan, number, config.concurrency)),
            campaign: campaign.clone(),
            number,
            repeat,
            concurrency: config.concurrency,
        };
        let batch_root = root.join(format!("batch-{number}"));
        private_directory(&batch_root, false)?;
        private_directory(&batch_root.join("starts"), false)?;
        private_directory(&batch_root.join("outcomes"), false)?;
        private_directory(&batch_root.join("allocations"), false)?;
        let prepared = batch_root.join("prepared.json");
        let dispatch = batch::Admission::new(
            config.concurrency,
            (1..=config.concurrency).collect(),
            reference.clone(),
            admission,
        )?;
        let batch_cassette = if recording.is_some() {
            Some(path_text(&super::recording::base(&root, number))?)
        } else if replay.is_some() {
            Some(replay::stage_batch(
                &root,
                &plan,
                number,
                config.concurrency,
            )?)
        } else {
            private_cassette.clone()
        };
        let settled = execution::BatchRun {
            config_path: &config_path,
            plan: &plan,
            root: &root,
            options: &options,
            identity,
            batch_root: &batch_root,
            material_path: prepared,
            finished_path: batch_root.join("finished.json"),
            generation: None,
            cassette: batch_cassette.as_deref(),
            provider_capture_path: recording
                .as_ref()
                .map(|_| super::recording::producer(&root, number, 1, config.concurrency)),
            recording: recording.clone(),
            reused: None,
        }
        .execute(dispatch, output, warnings)?;
        let execution::SettledBatch {
            prepared,
            failed: batch_failed,
            stopped: batch_stopped,
            reason,
            quiescent,
            completed,
        } = settled;
        completed_executions += completed;
        stopped = batch_stopped;
        producers_quiescent &= quiescent;
        if first_reason.is_none() {
            first_reason = reason.clone();
        }
        if batch_failed || stopped {
            failures += 1;
        }
        if stopped {
            break;
        }
        reference = prepared;
        completed_batches += 1;
    }
    let publication = recording
        .as_ref()
        .map(|recorder| recorder.finalize(producers_quiescent));
    let publication_status = match &publication {
        None => serde_json::Value::Null,
        Some(Ok(Some(published))) => {
            json!({"status":"published","format":"asb-zip-v1","path":published.destination,"tracks":published.manifest.tracks.len(),"cleanup_complete":published.cleanup_complete,"warnings":published.warnings})
        }
        Some(Ok(None)) => {
            json!({"status":"not-created","reason":"no verified recording material"})
        }
        Some(Err(error)) => json!({"status":"failed","reason":error.to_string()}),
    };
    publish(
        &root.join("finished.json"),
        &json!({"schema_version":1,"completed_batches":completed_batches,"failed_batches":failures,"stopped":stopped,"cassette_publication":publication_status}),
    )?;
    if publication.is_some() {
        if options.json {
            serde_json::to_writer(
                &mut *output,
                &json!({"type":"cassette-publication","publication":publication_status}),
            )
            .map_err(|error| Error::Io(error.to_string()))?;
            writeln!(output).map_err(io_error)?;
        } else if !options.quiet_output {
            match &publication {
                Some(Ok(Some(published))) => {
                    writeln!(
                        output,
                        "Cassette: {} (ZIP v1, {} tracks, published)",
                        published.destination.display(),
                        published.manifest.tracks.len()
                    )
                    .map_err(io_error)?;
                    for warning in &published.warnings {
                        writeln!(output, "Warning: {warning}").map_err(io_error)?;
                    }
                }
                Some(Ok(None)) => writeln!(
                    output,
                    "Recording was not created: no compatible workload material was prepared."
                )
                .map_err(io_error)?,
                Some(Err(_)) => writeln!(
                    output,
                    "Recording publication is not confirmed; partial evidence was retained."
                )
                .map_err(io_error)?,
                None => (),
            }
        }
    }
    if let Some(Err(error)) = publication {
        return Err(Error::Io(format!(
            "{}; {error}",
            first_reason
                .as_deref()
                .unwrap_or("recording publication failed")
        )));
    }
    if !options.json && options.quiet_output {
        writeln!(
            output,
            "Executions: completed={completed_executions} incomplete={}",
            total.saturating_sub(completed_executions),
        )
        .map_err(io_error)?;
    }
    if failures > 0 || stopped {
        if !options.quiet_output {
            Err(Error::Run(format!(
                "repeated execution had {failures} failed batch(es){}; {}; inspect {}",
                if stopped {
                    " and stopped further admission"
                } else {
                    ""
                },
                first_reason
                    .as_deref()
                    .unwrap_or("incomplete execution evidence"),
                root.join("plan.json").display()
            )))
        } else {
            Err(Error::Run(format!(
                "benchmark execution failed: {}; {} of {total} execution(s) completed; rerun with --verbose for retained evidence paths",
                first_reason
                    .as_deref()
                    .unwrap_or("incomplete execution evidence"),
                completed_executions,
            )))
        }
    } else {
        Ok(())
    }
}

fn must_stop(
    result: &Result<(), Error>,
    completed: usize,
    selected: usize,
    prepared: bool,
    cleanup: bool,
) -> bool {
    let ordinary_failure =
        matches!(result, Err(Error::Run(_))) && completed == selected && prepared && cleanup;
    (result.is_err() && !ordinary_failure) || !prepared || !cleanup || completed != selected
}

struct Observer {
    generation: Option<super::campaign::generation::StartReference>,
    root: PathBuf,
    directory: PathBuf,
    identity: BatchIdentity,
    completed: Mutex<BTreeSet<usize>>,
    cleanup_confirmed: AtomicBool,
}

impl replica::CompletionObserver for Observer {
    fn allocated(
        &self,
        replica: replica::Identity,
        reference: &crate::workloads::recovery::AttemptReference,
    ) -> Result<(), Error> {
        publish(
            &self
                .directory
                .join(format!("allocations/{}.json", replica.index)),
            &json!({"schema_version":1,"scope":"allocated-workload-reference-not-outcome","execution":self.identity.execution(replica),"reference":reference}),
        )
    }
    fn starting(
        &self,
        replica: replica::Identity,
        cohort: Option<&crate::measurements::ExecutionCohort>,
    ) -> Result<(), Error> {
        if !replica.valid() || replica.total != self.identity.concurrency {
            return Err(Error::Input(
                "campaign start differs from its original replica population".into(),
            ));
        }
        let cohort = cohort
            .filter(|cohort| cohort.admits(replica.index, replica.total))
            .ok_or_else(|| {
                Error::Input("campaign start requires its actual dispatch cohort".into())
            })?;
        publish(
            &self
                .directory
                .join(format!("starts/{}.json", replica.index)),
            &super::campaign::StartIntent {
                schema_version: 1,
                scope: "execution-start-intent-not-actual-attempt".into(),
                execution: self.identity.execution(replica),
                execution_cohort: Some(cohort.clone()),
                generation: self.generation.clone(),
            },
        )
    }
    fn completed(&self, replica: replica::Identity, record: &Path) -> Result<(), Error> {
        let mut completed = self
            .completed
            .lock()
            .map_err(|_| Error::Run("execution completion state was interrupted".into()))?;
        let relative = record
            .strip_prefix(&self.root)
            .map_err(|_| Error::Input("execution outcome is outside campaign".into()))?;
        let mut file = fs::File::open(record)
            .map_err(io_error)?
            .take(MAX_OUTCOME_BYTES + 1);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(io_error)?;
        if bytes.len() as u64 > MAX_OUTCOME_BYTES {
            return Err(Error::Input("execution outcome exceeds bounds".into()));
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::Input("invalid execution outcome".into()))?;
        let expected = self.identity.execution(replica);
        if value["summary"]["execution"]
            != serde_json::to_value(&expected).map_err(|error| Error::Input(error.to_string()))?
        {
            return Err(Error::Input(
                "execution outcome identity differs from planned slot".into(),
            ));
        }
        if !crate::workloads::ResourceDisposition::matches_record(
            value.get("workload_resource_disposition"),
            value["workload_cleanup_confirmed"] == true,
            &value["summary"],
        ) {
            return Err(Error::Input(
                "execution outcome resource disposition contradicts settlement evidence".into(),
            ));
        }
        publish(
            &self
                .directory
                .join(format!("outcomes/{}.json", replica.index)),
            &super::campaign::OutcomeReference {
                schema_version: 1,
                execution: expected,
                path: relative.to_path_buf(),
                bytes: bytes.len() as u64,
                sha256: crate::cassette::digest(&bytes),
                task: value["summary"]["task"].clone(),
            },
        )?;
        if value["workload_cleanup_confirmed"] != true || value["proxy_cleanup_confirmed"] != true {
            self.cleanup_confirmed.store(false, Ordering::Release);
        }
        completed.insert(replica.index);
        Ok(())
    }
}

fn path_text(path: &Path) -> Result<String, Error> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Input("execution paths must be UTF-8".into()))
}

fn private_directory(path: &Path, recursive: bool) -> Result<(), Error> {
    if recursive {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => return Ok(()),
            Ok(_) => {
                return Err(Error::Input(
                    "execution parent must be a real directory".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent = path
                    .parent()
                    .ok_or_else(|| Error::Input("invalid execution parent".into()))?;
                private_directory(parent, true)?;
            }
            Err(error) => return Err(io_error(error)),
        }
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(io_error)?;
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(io_error)?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(io_error)?;
    }
    Ok(())
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>, Error> {
    let bytes = serde_json::to_vec(value).map_err(|error| Error::Input(error.to_string()))?;
    if bytes.len() > MAX_PLAN_BYTES {
        return Err(Error::Input("execution metadata exceeds 16 MiB".into()));
    }
    Ok(bytes)
}

fn publish(path: &Path, value: &impl Serialize) -> Result<(), Error> {
    publish_bytes(path, &encode(value)?)
}

pub(crate) fn publish_bytes(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    publish_bytes_after_link(path, bytes, || Ok(()))
}

/// Shared publisher seam for private fault qualification; production callers
/// cannot select it through configuration, environment or CLI arguments.
pub(crate) fn publish_bytes_after_link(
    path: &Path,
    bytes: &[u8],
    after_link: impl FnOnce() -> Result<(), Error>,
) -> Result<(), Error> {
    let directory = path
        .parent()
        .ok_or_else(|| Error::Input("execution metadata needs a parent".into()))?;
    let temporary = directory.join(format!(
        ".{}.pending",
        crate::workloads::TaskIdentity::local("execution-metadata", None).attempt_id
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(io_error)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::hard_link(&temporary, path))
        .map_err(io_error)?;
    after_link()?;
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(io_error)?;
    fs::remove_file(temporary).map_err(io_error)?;
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(io_error)
}

#[cfg(test)]
mod tests {
    use super::super::replica::CompletionObserver;
    use super::*;

    #[test]
    fn input_admission_has_independent_exact_and_overflow_limits() {
        let maximum = InputCost {
            inputs: MAX_EXECUTIONS as usize,
            bytes: 2 * 1024 * 1024 * 1024,
            observed_bytes: 2 * 1024 * 1024 * 1024,
            files: 256,
            directory_bytes: 64 * 1024 * 1024,
            manifest_bytes: 256 * 1024 * 1024,
        };
        assert!(InputCost::default().checked_add(maximum).is_ok());
        // Track count is not a descriptor count: one archive may provide every
        // bounded selected input, rather than an artificial 256-track limit.
        assert!(
            InputCost::default()
                .checked_add(InputCost {
                    inputs: MAX_EXECUTIONS as usize,
                    files: 1,
                    ..Default::default()
                })
                .is_ok()
        );
        for field in 0..6 {
            let mut one = InputCost::default();
            match field {
                0 => one.inputs = 1,
                1 => one.bytes = 1,
                2 => one.observed_bytes = 1,
                3 => one.files = 1,
                4 => one.directory_bytes = 1,
                _ => one.manifest_bytes = 1,
            }
            assert!(maximum.checked_add(one).is_err());
            let mut overflow = InputCost::default();
            match field {
                0 => overflow.inputs = usize::MAX,
                1 => overflow.bytes = u64::MAX,
                2 => overflow.observed_bytes = u64::MAX,
                3 => overflow.files = usize::MAX,
                4 => overflow.directory_bytes = u64::MAX,
                _ => overflow.manifest_bytes = u64::MAX,
            }
            assert!(overflow.checked_add(one).is_err());
        }
    }

    fn planning_fixture() -> (PathBuf, BenchmarkConfig, PathBuf) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/campaign-planning-fixtures")
            .join(crate::workloads::TaskIdentity::local("planning", None).attempt_id);
        fs::create_dir_all(root.join(".asb")).unwrap();
        let root = root.canonicalize().unwrap();
        fs::write(
            root.join(".asb/project.toml"),
            toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
        )
        .unwrap();
        let config: BenchmarkConfig = toml::from_str(
            "agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='bug-fix'\nconcurrency=2",
        )
        .unwrap();
        let config_path = root.join("benchmark.toml");
        fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        (root, config, config_path)
    }

    #[test]
    fn completed_only_continuation_validates_population_and_never_creates_runtime_state() {
        use crate::runner::campaign::generation::{Blob, StartReference};
        use serde_json::Value;
        use std::collections::BTreeMap;
        struct BrokenOutput;
        impl Write for BrokenOutput {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for case in [
            "complete",
            "failed",
            "missing",
            "extra",
            "wrong-slot",
            "writer",
            "presentation",
        ] {
            let (root, config, path) = planning_fixture();
            let prepared = Prepared::prepare(
                &path,
                config,
                1,
                RunOptions {
                    mode: Some(crate::tape::Mode::Record),
                    ..Default::default()
                },
                Some(&root.join("campaigns")),
            )
            .unwrap();
            let campaign = prepared.root.clone();
            let mut completed = BTreeMap::from([
                (
                    1,
                    json!({"process_success":true,"workload_success":case != "failed","task":{"task_id":"first"}}),
                ),
                (
                    2,
                    json!({"process_success":true,"workload_success":true,"task":{"task_id":"second"}}),
                ),
            ]);
            if matches!(case, "missing" | "wrong-slot") {
                completed.remove(&2);
            }
            if matches!(case, "extra" | "wrong-slot") {
                completed.insert(3, completed[&1].clone());
            }
            let recording = (case == "writer").then(|| {
                Arc::new(super::super::recording::Recorder::new(
                    campaign.join("unexpected.zip"),
                    campaign.clone(),
                    prepared.plan.campaign.clone(),
                    1,
                    2,
                    Some(&prepared.plan),
                ))
            });
            let mut bytes = Vec::new();
            let mut broken = BrokenOutput;
            let result = continue_campaign(
                Continuation {
                    invocation: path,
                    root: campaign.clone(),
                    plan: prepared.plan,
                    completed,
                    pending: Vec::new(),
                    compatibility: None,
                    reference: StartReference {
                        generation: "unused-generation".into(),
                        configuration: 0,
                        manifest: Blob::of(b"test"),
                        admission: Blob::of(b"test"),
                    },
                    recording,
                },
                true,
                false,
                None,
                None,
                if case == "presentation" {
                    &mut broken
                } else {
                    &mut bytes
                },
                &Mutex::new(BTreeSet::new()),
            );
            match case {
                "complete" => assert!(result.is_ok()),
                "failed" => assert!(matches!(result, Err(Error::Run(_)))),
                "presentation" => assert!(matches!(result, Err(Error::Io(_)))),
                _ => {
                    assert!(matches!(result, Err(Error::Input(_))));
                    assert!(bytes.is_empty());
                }
            }
            if matches!(case, "complete" | "failed") {
                let rows: Vec<Value> = String::from_utf8(bytes)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(rows.len(), 2);
                assert!(rows.iter().all(|row| row["reused"] == true));
            }
            assert!(!campaign.exists(), "{case} must not allocate runtime state");
            crate::fixture_writer::cleanup(&root).unwrap();
        }
    }

    #[test]
    fn published_internal_recording_owns_root_without_starting_a_journal_or_client() {
        let (root, config, path) = planning_fixture();
        let parent = root.join("isolated/campaigns");
        let prepared = Prepared::prepare(
            &path,
            config.clone(),
            2,
            RunOptions {
                mode: Some(crate::tape::Mode::Record),
                ..Default::default()
            },
            Some(&parent),
        )
        .unwrap();
        assert_eq!(
            prepared.plan.recording.as_ref().unwrap(),
            &prepared.root.join("recording.asb.zip")
        );
        assert!(!parent.exists());
        let published = publish_campaign(prepared).unwrap();
        let campaign = published.root.clone();
        let lock = campaign.join(".owner.lock");
        assert!(campaign.join("plan.json").is_file());
        assert!(!campaign.join("recording.asb.zip").exists());
        assert!(!campaign.join(".recording.asb.zip.partial").exists());
        assert!(!campaign.join("batch-1").exists());
        assert!(crate::output_lock::OutputLock::acquire(&lock, "busy").is_err());
        drop(published);
        let owner = crate::output_lock::OutputLock::acquire(&lock, "busy").unwrap();
        assert!(campaign.join("plan.json").is_file());
        drop(owner);

        struct BrokenOutput(PathBuf);
        impl Write for BrokenOutput {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                assert!(crate::output_lock::OutputLock::acquire(&self.0, "busy").is_err());
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "publication-only fixture output closed",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let prepared = Prepared::prepare(
            &path,
            config,
            1,
            RunOptions {
                mode: Some(crate::tape::Mode::Record),
                ..Default::default()
            },
            Some(&parent),
        )
        .unwrap();
        let campaign = prepared.root.clone();
        let published = publish_campaign(prepared).unwrap();
        let error = execute(
            published,
            &mut BrokenOutput(campaign.join(".owner.lock")),
            &Mutex::new(BTreeSet::new()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("fixture output closed"));
        assert!(!campaign.join("batch-1").exists());
        assert!(!campaign.join(".recording.asb.zip.partial").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn later_publication_failure_retains_earlier_plan_and_cannot_execute_any_campaign() {
        let (root, config, path) = planning_fixture();
        let parent = root.join("isolated/campaigns");
        let first = Prepared::prepare(
            &path,
            config.clone(),
            1,
            RunOptions {
                mode: Some(crate::tape::Mode::Record),
                ..Default::default()
            },
            Some(&parent),
        )
        .unwrap();
        let first_root = first.root.clone();
        let second = Prepared::prepare(
            &path,
            config.clone(),
            1,
            RunOptions {
                mode: Some(crate::tape::Mode::Record),
                ..Default::default()
            },
            Some(&parent),
        )
        .unwrap();
        fs::create_dir_all(&second.root).unwrap();
        let foreign = second.plan.recording.as_ref().unwrap().clone();
        fs::write(&foreign, b"foreign cassette").unwrap();
        let second_root = second.root.clone();
        let executions = std::cell::Cell::new(0);
        let result = (|| -> Result<(), Error> {
            let all = [first, second]
                .into_iter()
                .map(publish_campaign)
                .collect::<Result<Vec<_>, _>>()?;
            for published in all {
                executions.set(executions.get() + 1);
                execute(published, &mut Vec::new(), &Mutex::new(BTreeSet::new()))?;
            }
            Ok(())
        })();
        assert!(result.unwrap_err().to_string().contains("already exists"));
        assert_eq!(executions.get(), 0);
        assert!(first_root.join("plan.json").is_file());
        assert!(!first_root.join("batch-1").exists());
        assert!(!first_root.join(".recording.asb.zip.partial").exists());
        assert!(!second_root.join("plan.json").exists());
        assert_eq!(fs::read(foreign).unwrap(), b"foreign cassette");
        let owner =
            crate::output_lock::OutputLock::acquire(&first_root.join(".owner.lock"), "busy")
                .unwrap();
        drop(owner);

        let prepared =
            Prepared::prepare(&path, config, 1, RunOptions::default(), Some(&parent)).unwrap();
        let foreign_root = prepared.root.clone();
        fs::create_dir_all(&foreign_root).unwrap();
        fs::write(foreign_root.join("sentinel"), b"untouched").unwrap();
        assert!(publish_campaign(prepared).is_err());
        assert_eq!(
            fs::read(foreign_root.join("sentinel")).unwrap(),
            b"untouched"
        );
        assert!(!foreign_root.join("plan.json").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn campaign_planning_has_no_output_side_effects_and_activation_rechecks_destination() {
        let (root, config, config_path) = planning_fixture();
        let parent = root.join("isolated/sweep-configuration/campaigns");
        let destination = root.join("recordings/new/trials.asb.zip");
        let (prepared, saved_policy) = {
            let cassette = destination.to_string_lossy().into_owned();
            let measurements = root
                .join("external-measures")
                .to_string_lossy()
                .into_owned();
            let native_capsule = root
                .join("recordings/new/native-capsule")
                .to_string_lossy()
                .into_owned();
            let policy = crate::planning::resolve_for_run(&config, true).unwrap();
            let prepared = Prepared::prepare(
                &config_path,
                config.clone(),
                3,
                RunOptions {
                    mode: Some(crate::tape::Mode::Record),
                    cassette: Some(&cassette),
                    measurement_dir: Some(&measurements),
                    policy: Some(&policy),
                    tool_mode: Some(crate::tape::Mode::Record),
                    tool_mode_explicit: true,
                    mcp_mode: Some(crate::tape::Mode::Record),
                    native_capsule_record: Some(&native_capsule),
                    ..Default::default()
                },
                Some(&parent),
            )
            .unwrap();
            (prepared, policy)
        }; // Temporary option strings are gone; policy is no longer borrowed.
        assert_eq!(prepared.options.policy.as_ref(), Some(&saved_policy));
        assert!(prepared.options.measurement_override);
        assert_eq!(prepared.options.tool_mode, Some(crate::tape::Mode::Record));
        assert!(prepared.options.tool_mode_explicit);
        assert_eq!(prepared.options.mcp_mode, Some(crate::tape::Mode::Record));
        assert_eq!(
            prepared.options.native_capsule_record.as_deref(),
            Some(
                root.join("recordings/new/native-capsule")
                    .to_string_lossy()
                    .as_ref()
            )
        );
        assert!(prepared.options.native_capsule_replay.is_none());
        assert_eq!(
            prepared.plan.measurement_root.as_ref().unwrap(),
            &root.join("external-measures").join(&prepared.plan.campaign)
        );
        assert!(!root.join("external-measures").exists());
        assert_eq!(prepared.plan.validate_layout().unwrap(), 6);
        assert_eq!(prepared.root.parent().unwrap(), parent);
        assert_eq!(prepared.config_path, config_path);
        assert!(prepared.config_path.is_absolute());
        assert_eq!(prepared.plan.configuration, config);
        assert_eq!(prepared.plan.recording.as_ref().unwrap(), &destination);
        assert!(!parent.exists());
        assert!(!root.join("recordings").exists());
        assert!(!root.join("results").exists());
        #[cfg(unix)]
        {
            let other = root.join("other-project");
            fs::create_dir_all(other.join(".asb")).unwrap();
            fs::copy(
                root.join(".asb/project.toml"),
                other.join(".asb/project.toml"),
            )
            .unwrap();
            let other_config = other.join("benchmark.toml");
            fs::copy(&config_path, &other_config).unwrap();
            let alias = root.join("linked-benchmark.toml");
            std::os::unix::fs::symlink(&other_config, &alias).unwrap();
            let linked =
                Prepared::prepare(&alias, config.clone(), 1, RunOptions::default(), None).unwrap();
            assert_eq!(linked.config_path, alias);
            assert_eq!(linked.plan.source, other_config);
            assert_eq!(linked.parent, root.join("results/asb-executions"));
        }
        let planned_root = prepared.root.clone();
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"foreign cassette created after planning").unwrap();
        let error = activate(prepared, &mut Vec::new(), &Mutex::new(BTreeSet::new())).unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert!(!planned_root.exists());
        assert!(!parent.exists());
        assert_eq!(
            fs::read(&destination).unwrap(),
            b"foreign cassette created after planning"
        );
        let live = Prepared::prepare(&config_path, config, 2, RunOptions::default(), None).unwrap();
        assert_eq!(live.plan.validate_layout().unwrap(), 4);
        assert_eq!(live.parent, root.join("results/asb-executions"));
        assert!(!live.root.exists());
        drop(live);
        assert!(!root.join("results").exists());
        fs::remove_dir_all(root).unwrap();
    }

    fn outcome_observer() -> (PathBuf, Observer, replica::Identity) {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("repeat-settlement", None).attempt_id);
        private_directory(&root, false).unwrap();
        private_directory(&root.join("outcomes"), false).unwrap();
        let observer = Observer {
            generation: None,
            root: root.clone(),
            directory: root.clone(),
            identity: BatchIdentity {
                replay: None,
                campaign: "fixture".into(),
                number: 1,
                repeat: 2,
                concurrency: 1,
            },
            completed: Mutex::new(BTreeSet::new()),
            cleanup_confirmed: AtomicBool::new(true),
        };
        (root, observer, replica::Identity::default())
    }

    #[test]
    fn start_intent_binds_actual_partial_cohort_without_renumbering_original_replicas() {
        let (root, mut observer, _) = outcome_observer();
        private_directory(&root.join("starts"), false).unwrap();
        observer.identity.concurrency = 4;
        let replica = replica::Identity { index: 2, total: 4 };
        let cohort = crate::measurements::ExecutionCohort {
            id: "abc-123".into(),
            selected_replicas: 4,
            admitted_slots: vec![2, 4],
            dispatch: None,
        };
        let path = root.join("starts/2.json");
        assert!(observer.starting(replica, None).is_err());
        for changed in [
            crate::measurements::ExecutionCohort {
                admitted_slots: vec![1, 4],
                ..cohort.clone()
            },
            crate::measurements::ExecutionCohort {
                selected_replicas: 2,
                ..cohort.clone()
            },
            crate::measurements::ExecutionCohort {
                id: String::new(),
                ..cohort.clone()
            },
        ] {
            assert!(observer.starting(replica, Some(&changed)).is_err());
        }
        assert!(
            observer
                .starting(replica::Identity { index: 2, total: 2 }, Some(&cohort))
                .is_err()
        );
        assert!(!path.exists());
        observer.starting(replica, Some(&cohort)).unwrap();
        let original = fs::read(&path).unwrap();
        let intent: super::super::campaign::StartIntent =
            serde_json::from_slice(&original).unwrap();
        assert_eq!(intent.execution, observer.identity.execution(replica));
        assert_eq!(intent.execution_cohort, Some(cohort.clone()));
        assert!(observer.starting(replica, Some(&cohort)).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        // Old start declarations remain inspectable with explicitly unknown
        // cohort provenance; deserialization alone grants no continuation.
        let mut historical: serde_json::Value = serde_json::from_slice(&original).unwrap();
        historical
            .as_object_mut()
            .unwrap()
            .remove("execution_cohort");
        let old: super::super::campaign::StartIntent = serde_json::from_value(historical).unwrap();
        assert!(old.execution_cohort.is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn grading_and_settlement_dispositions_are_independent_admission_evidence() {
        for (disposition, confirmed, summary_disposition, accepted, continue_batch) in [
            ("not-required", true, "not-required", true, true),
            ("removed", true, "removed", true, true),
            ("not-created", true, "not-created", true, true),
            ("stopped-preserved", true, "stopped-preserved", true, true),
            (
                "stopped-preserved",
                false,
                "stopped-preserved",
                false,
                false,
            ),
            ("stopped-preserved", true, "removed", false, false),
            ("unknown", true, "unknown", false, false),
        ] {
            let (root, observer, replica) = outcome_observer();
            let value = json!({"summary":{"execution":observer.identity.execution(replica),
                "task":{"attempt_id":"failed-attempt"},"workload_success":null,
                "workload_failure":{"kind":"grader_error"},"workload_resource_disposition":summary_disposition},
                "workload_resource_disposition":disposition,"workload_cleanup_confirmed":confirmed,"proxy_cleanup_confirmed":true});
            publish(&root.join("record.json"), &value).unwrap();
            assert_eq!(
                observer
                    .completed(replica, &root.join("record.json"))
                    .is_ok(),
                accepted,
                "{disposition}"
            );
            assert_eq!(
                !must_stop(
                    &Err(Error::Run("grader failure".into())),
                    observer.completed.lock().unwrap().len(),
                    1,
                    true,
                    observer.cleanup_confirmed.load(Ordering::Acquire)
                ),
                continue_batch
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn unconfirmed_cleanup_preserves_outcome_but_stops_new_admission() {
        for field in ["workload_cleanup_confirmed", "proxy_cleanup_confirmed"] {
            let root = std::env::temp_dir()
                .join(crate::workloads::TaskIdentity::local("repeat-cleanup", None).attempt_id);
            private_directory(&root, false).unwrap();
            private_directory(&root.join("outcomes"), false).unwrap();
            let observer = Observer {
                generation: None,
                root: root.clone(),
                directory: root.clone(),
                identity: BatchIdentity {
                    replay: None,
                    campaign: "fixture".into(),
                    number: 1,
                    repeat: 2,
                    concurrency: 1,
                },
                completed: Mutex::new(BTreeSet::new()),
                cleanup_confirmed: AtomicBool::new(true),
            };
            let replica = replica::Identity::default();
            let mut outcome = json!({"summary":{"execution":observer.identity.execution(replica),"task":{"attempt_id":"graded-fixture"}},"workload_cleanup_confirmed":true,"proxy_cleanup_confirmed":true});
            outcome[field] = json!(false);
            publish(&root.join("record.json"), &outcome).unwrap();
            observer
                .completed(replica, &root.join("record.json"))
                .unwrap();
            assert_eq!(observer.completed.lock().unwrap().len(), 1);
            assert!(root.join("outcomes/1.json").exists());
            assert!(must_stop(
                &Err(Error::Run("cleanup failure".into())),
                1,
                1,
                true,
                observer.cleanup_confirmed.load(Ordering::Acquire)
            ));
            assert!(must_stop(
                &Err(Error::Io("output failure".into())),
                1,
                1,
                true,
                true
            ));
            assert!(!must_stop(
                &Err(Error::Run("ordinary graded failure".into())),
                1,
                1,
                true,
                true
            ));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn metadata_publication_never_replaces_and_legacy_input_size_is_bounded() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("repeat-metadata", None).attempt_id);
        private_directory(&root, false).unwrap();
        let destination = root.join("plan.json");
        publish(&destination, &json!({"original":true})).unwrap();
        let bytes = fs::read(&destination).unwrap();
        assert!(publish(&destination, &json!({"original":false})).is_err());
        assert_eq!(fs::read(&destination).unwrap(), bytes);
        let large = root.join("large.jsonl");
        fs::File::create(&large)
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        assert!(legacy::transfer_input(&large, &mut std::io::sink()).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn repeated_campaign_rejects_an_unsupported_max_step_limit_before_preparation() {
        let (root, config, path) = planning_fixture();
        let result = Inspection::prepare_selected(
            &path,
            config,
            1,
            RunOptions {
                max_steps: Some(100),
                ..Default::default()
            },
            Some(&root.join("campaigns")),
            None,
        );
        let error = match result {
            Ok(_) => panic!("unsupported max-step limit unexpectedly prepared a campaign"),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            "agent `pi` does not support --max-steps; remove the flag or select an adapter with an enforced turn limit"
        );
        assert!(!root.join("campaigns").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
