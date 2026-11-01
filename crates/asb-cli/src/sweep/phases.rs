//! Consuming sweep admission, activation, joined execution and finalization.
//! Rejected continuation never activates or finalizes historical output.
use super::presentation::Presentation;
use super::*;

enum PreparedMode {
    Fresh,
    Continued(Box<checkpoint::campaigns::PreparedContinuation>),
    Resumed {
        checkpoint: Box<checkpoint::Inputs>,
        reused: resume::State,
    },
}
enum ActiveMode {
    Continued(checkpoint::campaigns::Guard),
    Campaigns(Option<checkpoint::Inputs>),
    Fresh(Option<checkpoint::Inputs>),
    Resumed(checkpoint::Inputs),
    ReusedCampaigns(checkpoint::Inputs),
}
impl ActiveMode {
    fn checkpoint(&self) -> Option<&checkpoint::Inputs> {
        match self {
            // Legacy completion observers must never consume campaign inputs.
            Self::Campaigns(_) | Self::ReusedCampaigns(_) | Self::Continued(_) => None,
            Self::Fresh(value) => value.as_ref(),
            Self::Resumed(value) => Some(value),
        }
    }
}
pub(super) struct PreparedSweep<'a> {
    campaigns: Option<campaigns::Prepared>,
    source: String,
    parent: PathBuf,
    stem: String,
    plan: plan::SweepPlan,
    weights: Vec<usize>,
    reservations: Vec<crate::planning::Resources>,
    scheduling: scheduler::Scheduling,
    options: Options<'a>,
    results: PathBuf,
    backup: PathBuf,
    replay_inputs: Option<PathBuf>,
    replay_sources: Vec<PathBuf>,
    pricing_snapshots: Vec<(String, serde_json::Value)>,
    restore_recordings: bool,
    mode: PreparedMode,
    _output_lock: lock::OutputLock,
}
impl<'a> PreparedSweep<'a> {
    pub fn prepare(
        source: &str,
        options: Options<'a>,
        output_lock: lock::OutputLock,
        sweep: SweepFile,
        output: &mut dyn std::io::Write,
    ) -> Result<Self, Error> {
        let sweep_path = Path::new(source);
        let parent = sweep_path.parent().unwrap_or_else(|| Path::new("."));
        let stem = sweep_path
            .file_stem()
            .and_then(|v| v.to_str())
            .filter(|v| !v.is_empty())
            .unwrap_or("sweep");
        let mode = options.mode;
        let cassette_root = options.cassette_root;
        let mut plan = plan::build(parent, &sweep)?;
        plan.execution_mode = match mode {
            None => plan::SavedExecutionMode::Live,
            Some(crate::tape::Mode::Record) => plan::SavedExecutionMode::Record,
            Some(crate::tape::Mode::Replay) => plan::SavedExecutionMode::Replay,
        };
        plan.respect_record_timings = options.respect_record_timings;
        let weights: Vec<_> = plan
            .entries
            .iter()
            .map(|entry| entry.configuration.concurrency)
            .collect();
        let scheduling =
            scheduler::Scheduling::new(options.jobs, options.max_active_attempts, &weights)?;
        plan.scheduling = Some(scheduling);
        let reservations = plan.resource_costs(sweep.resources)?;
        plan.resources = sweep.resources;
        let results = parent.join(format!("{stem}-results"));
        let backup = parent.join(format!("{stem}-history")).join(&plan.run_id);
        let campaigns = if options.resume {
            None
        } else {
            campaigns::prepare(
                parent,
                &results,
                &backup.join(format!("{stem}-results")),
                &mut plan,
                options,
            )?
        };
        if options.resume {
            let results = parent.join(format!("{stem}-results"));
            if !std::fs::symlink_metadata(&results).is_ok_and(|metadata| metadata.is_dir()) {
                return Err(Error::Input("cannot resume: the existing sweep results must be a real directory; run without --resume to start a fresh sweep".into()));
            }
            let saved = plan::SweepPlan::load(&results, parent, &sweep)?.ok_or_else(|| {
                Error::Input("cannot resume: the original sweep plan is missing".into())
            })?;
            if saved.execution.is_some() && options.reconcile_completed {
                return Err(Error::Input("campaign continuation requires saved-slot validation; existing results were not changed".into()));
            }
            campaigns::validate_resume_options(&results, &saved, options)?;
            if saved.execution_mode != plan.execution_mode
                || saved.respect_record_timings != plan.respect_record_timings
                || saved.scheduling != plan.scheduling
                || saved.resources != plan.resources
                || saved
                    .entries
                    .iter()
                    .zip(&plan.entries)
                    .any(|(old, current)| {
                        old.selection != current.selection || old.policy != current.policy
                    })
            {
                return Err(Error::Input("cannot resume: dataset selection, execution mode, replay timing or scheduling differs from the original sweep; use the original options or start a fresh sweep".into()));
            }
            plan = saved;
        }
        // Validate cassette and continuation inputs first.  A malformed replay
        // source is a deterministic invocation error, not a capacity decision;
        // capacity is still checked before dependency preparation or output
        // publication can start.
        crate::disk_space::confirm_capacity(
            parent,
            crate::disk_space::sweep(
                plan.entries
                    .iter()
                    .map(|entry| (&entry.configuration, entry.policy.as_ref())),
                options.repeat.unwrap_or(1),
                mode,
            ),
            options.interactive && !options.json,
            options.json,
            output,
        )?;
        // Warm every selected execution dependency before moving old output or
        // taking input snapshots. This is execution preflight, never optional
        // metadata discovery: an automatic project-local install after the
        // initial seal would make ASB's own installation look like an external
        // input mutation. Deduplicate over the full population, not once per
        // task.
        let mut prepared_execution = BTreeSet::new();
        let mut prepared_workloads = BTreeSet::new();
        for entry in plan.entries.iter().filter(|_| !options.resume) {
            let (root, project) = crate::project::load(&parent.join(&entry.reference))?;
            let preparation_key = crate::install::execution_requirements(&entry.configuration);
            if prepared_execution.insert((root.clone(), preparation_key)) {
                let mut messages = Vec::new();
                crate::install::ensure_execution_requirements(
                    &entry.configuration,
                    &parent.join(&entry.reference),
                    &mut messages,
                )?;
                if !options.json {
                    output.write_all(&messages).map_err(crate::io_error)?;
                }
            }
            if prepared_workloads.insert((
                root.clone(),
                entry.configuration.workload.clone(),
                entry.configuration.agent.clone(),
            )) {
                crate::workloads::preflight(
                    &entry.configuration.workload,
                    &crate::workloads::PreflightContext {
                        project_root: &root,
                        project_config: &project,
                        agent: &entry.configuration.agent,
                    },
                )?;
            }
        }
        // Live/record pricing is captured before rotating old results. Replay
        // is offline and must not fetch today's unrelated prices. Existing
        // snapshots remain with their original results in sweep history.
        let pricing_snapshots = if options.resume || mode == Some(crate::tape::Mode::Replay) {
            Vec::new()
        } else {
            capture_pricing_snapshots(&plan)?
        };
        let old_results = parent.join(format!("{stem}-results")).canonicalize().ok();
        let relocated_input = if mode == Some(crate::tape::Mode::Replay) {
            cassette_root
                .and_then(|root| root.canonicalize().ok())
                .and_then(|root| {
                    old_results
                        .as_ref()
                        .and_then(|old| root.strip_prefix(old).ok().map(Path::to_path_buf))
                })
        } else {
            None
        };

        let results = parent.join(format!("{stem}-results"));
        let backup = parent.join(format!("{stem}-history")).join(&plan.run_id);
        let replay_inputs = relocated_input
            .as_ref()
            .map(|relative| backup.join(format!("{stem}-results")).join(relative))
            .or_else(|| cassette_root.map(Path::to_path_buf));
        let preserved_source = backup.join(format!("{stem}-results"));
        let replay_sources =
            execution::replay_sources(&plan, replay_inputs.as_deref().unwrap_or(&preserved_source));
        let prepared_mode = if options.resume {
            let checkpoint =
                checkpoint::Inputs::resume(parent, &results, &plan, mode, &replay_sources)?;
            if options.reconcile_completed {
                checkpoint.reconcile_completed(parent, &results, &plan)?;
            }
            if plan.execution.is_some() {
                match checkpoint.prepare_campaign_continuation(parent, &results, &plan)? {
                    checkpoint::campaigns::CampaignContinuation::Completed(checkpoint, reused) => {
                        PreparedMode::Resumed { checkpoint, reused }
                    }
                    checkpoint::campaigns::CampaignContinuation::Pending(prepared) => {
                        PreparedMode::Continued(prepared)
                    }
                }
            } else {
                let reused = checkpoint.resume_outcomes(parent, &results, &plan)?;
                PreparedMode::Resumed {
                    checkpoint: Box::new(checkpoint),
                    reused,
                }
            }
        } else {
            PreparedMode::Fresh
        };
        Ok(Self {
            campaigns,
            source: source.into(),
            parent: parent.into(),
            stem: stem.into(),
            plan,
            weights,
            reservations,
            scheduling,
            options,
            results,
            backup,
            replay_inputs,
            replay_sources,
            pricing_snapshots,
            restore_recordings: cassette_root.is_none()
                || relocated_input
                    .as_ref()
                    .is_some_and(|p| p.as_os_str().is_empty()),
            mode: prepared_mode,
            _output_lock: output_lock,
        })
    }
    pub fn activate(self) -> Result<ActivatedSweep<'a>, Error> {
        let mut campaigns = None;
        let (mode, reused, initial_warning) = match self.mode {
            PreparedMode::Continued(prepared) => {
                let (guard, admitted) = prepared.publish(&self.results, &self.plan)?;
                campaigns = Some(campaigns::Pool::continued(
                    admitted,
                    &self.plan,
                    self.options,
                ));
                (ActiveMode::Continued(guard), resume::State::default(), None)
            }
            PreparedMode::Resumed { checkpoint, reused } => {
                let mode = if self.plan.execution.is_some() {
                    ActiveMode::ReusedCampaigns(*checkpoint)
                } else {
                    ActiveMode::Resumed(*checkpoint)
                };
                (mode, reused, None)
            }
            PreparedMode::Fresh => {
                plan::fresh_results(&self.parent, &self.stem, &self.plan)?;
                // The values were fetched during preflight, before this
                // activation moved any historical results. Retain exactly
                // those values for offline comparison.
                for (name, pricing) in &self.pricing_snapshots {
                    crate::evidence::write_json(&self.results.join(name), pricing)
                        .map_err(Error::Io)?;
                }
                if let Some(prepared) = self.campaigns {
                    campaigns = Some(prepared.publish(&self.results)?);
                    #[cfg(all(test, unix))]
                    tests::before_initial_seal(&self.results);
                    match checkpoint::Inputs::begin(
                        &self.parent,
                        &self.results,
                        &self.plan,
                        self.options.mode,
                        &[],
                    ) {
                        Ok(checkpoint) => (
                            ActiveMode::Campaigns(Some(checkpoint)),
                            resume::State::default(),
                            None,
                        ),
                        Err(error) => (
                            ActiveMode::Campaigns(None),
                            resume::State::default(),
                            Some(error),
                        ),
                    }
                } else {
                    #[cfg(all(test, unix))]
                    tests::before_initial_seal(&self.results);
                    match checkpoint::Inputs::begin(
                        &self.parent,
                        &self.results,
                        &self.plan,
                        self.options.mode,
                        &self.replay_sources,
                    ) {
                        Ok(checkpoint) => (
                            ActiveMode::Fresh(Some(checkpoint)),
                            resume::State::default(),
                            None,
                        ),
                        Err(error) => (
                            ActiveMode::Fresh(None),
                            resume::State::default(),
                            Some(error),
                        ),
                    }
                }
            }
        };
        Ok(ActivatedSweep {
            campaigns,
            source: self.source,
            parent: self.parent,
            plan: self.plan,
            weights: self.weights,
            reservations: self.reservations,
            scheduling: self.scheduling,
            options: self.options,
            results: self.results,
            backup: self.backup,
            replay_inputs: self.replay_inputs,
            restore_recordings: self.restore_recordings,
            mode,
            reused,
            initial_warning,
            warnings: Mutex::new(BTreeSet::new()),
            _output_lock: self._output_lock,
        })
    }
}

fn capture_pricing_snapshots(
    plan: &plan::SweepPlan,
) -> Result<Vec<(String, serde_json::Value)>, Error> {
    #[cfg(all(test, unix))]
    tests::before_pricing_snapshot()?;
    let providers = plan
        .entries
        .iter()
        .map(|entry| entry.configuration.provider.as_str())
        .collect::<BTreeSet<_>>();
    let mut snapshots = Vec::new();
    for provider in providers {
        if crate::providers::credential(provider).is_none() {
            continue;
        }
        if let Some((name, pricing)) = crate::providers::pricing_snapshot(provider)? {
            snapshots.push((name.to_owned(), pricing));
        }
    }
    Ok(snapshots)
}

#[cfg(all(test, unix))]
#[path = "../../tests/unit/sweep_phases.rs"]
mod tests;
pub(super) struct ActivatedSweep<'a> {
    campaigns: Option<campaigns::Pool>,
    source: String,
    parent: PathBuf,
    plan: plan::SweepPlan,
    weights: Vec<usize>,
    reservations: Vec<crate::planning::Resources>,
    scheduling: scheduler::Scheduling,
    options: Options<'a>,
    results: PathBuf,
    backup: PathBuf,
    replay_inputs: Option<PathBuf>,
    restore_recordings: bool,
    mode: ActiveMode,
    reused: resume::State,
    initial_warning: Option<Error>,
    warnings: Mutex<BTreeSet<String>>,
    _output_lock: lock::OutputLock,
}
pub(super) struct SettledSweep<'a> {
    activated: ActivatedSweep<'a>,
    failures: Vec<(String, Vec<String>)>,
    schedule_error: Option<Error>,
}

impl<'a> ActivatedSweep<'a> {
    pub fn execute(mut self, output: &mut Presentation<'_>) -> SettledSweep<'a> {
        let mut failures = Vec::new();
        let mut owned_spools = Vec::new();
        let mut schedule_error = None;
        if let Some(error) = self.initial_warning.take() {
            schedule_error = crate::runner::diagnostics::Warning::new(
                crate::runner::diagnostics::Stage::InputInitial,
                Path::new(&self.source),
                &self.results.join(".checkpoint/inputs.json"),
                &error,
            )
            .emit(self.options.json, output, &self.warnings)
            .err();
        }
        if !output.failed()
            && !self.options.json
            && !self.options.resume
            && self.backup.is_dir()
            && let Err(error) = writeln!(
                output,
                "Previous sweep results were preserved under {}.",
                self.backup.display()
            )
        {
            schedule_error = Some(crate::io_error(error));
        }
        let execution = execution::Execution {
            campaigns: self.campaigns.as_ref(),
            source: &self.source,
            parent: &self.parent,
            plan: &self.plan,
            results: &self.results,
            backup: &self.backup,
            replay_inputs: self.replay_inputs.as_deref(),
            restore_recordings: self.restore_recordings,
            options: self.options,
            warnings: &self.warnings,
            checkpoint: self.mode.checkpoint(),
            reused: &self.reused,
        };
        if !output.failed() {
            schedule_error = if self.scheduling.jobs == 1 {
                let mut failure = None;
                for index in 0..self.plan.entries.len() {
                    if !self.options.json
                        && let Err(error) = write_progress(
                            output,
                            index,
                            self.plan.entries.len(),
                            &execution.config_path(index),
                            &self.plan.entries[index].configuration,
                        )
                    {
                        failure = Some(error);
                        break;
                    }
                    if let Err(error) = execution.run_to(index, output) {
                        add_failure(
                            &mut failures,
                            execution.config_path(index).display().to_string(),
                            error.to_string(),
                        );
                        if self.options.continue_after_failures.is_some_and(|limit| {
                            failures
                                .iter()
                                .map(|(_, configs)| configs.len())
                                .sum::<usize>()
                                > limit
                        }) {
                            break;
                        }
                    }
                    if output.failed() {
                        failure = output.error();
                        break;
                    }
                    if execution.stop_requested(index) {
                        break;
                    }
                }
                failure
            } else {
                let mut started = 0;
                let continued_failures =
                    std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let failure_budget = self.options.continue_after_failures;
                let stopping_failures = continued_failures.clone();
                scheduler::run_with_resources_stopping_on(
                    self.scheduling,
                    &self.weights,
                    self.plan.resources,
                    &self.reservations,
                    |index| Ok(execution.run(index)),
                    |report| {
                        let failed = match report {
                            Ok(report) => report.result.is_err(),
                            Err(_) => true,
                        };
                        if failed {
                            let failures = stopping_failures
                                .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                                + 1;
                            if failure_budget.is_some_and(|limit| failures > limit) {
                                return true;
                            }
                        }
                        report.is_ok_and(|report| {
                            report.output_error.is_some() || report.stop_admission
                        })
                    },
                    |event| {
                        match event {
                            scheduler::Event::Started(index) => {
                                if !self.options.json {
                                    write_progress(
                                        output,
                                        started,
                                        self.plan.entries.len(),
                                        &execution.config_path(index),
                                        &self.plan.entries[index].configuration,
                                    )?;
                                }
                                started += 1;
                            }
                            scheduler::Event::Finished(index, report) => {
                                let report = match report {
                                    Ok(report) => report,
                                    Err(error) => {
                                        add_failure(
                                            &mut failures,
                                            execution.config_path(index).display().to_string(),
                                            error.to_string(),
                                        );
                                        return Ok(());
                                    }
                                };
                                if report.spool_created {
                                    owned_spools.push(index);
                                }
                                if let Err(error) = report.result {
                                    add_failure(
                                        &mut failures,
                                        execution.config_path(index).display().to_string(),
                                        error.to_string(),
                                    );
                                }
                                if let Some(error) = report.output_error {
                                    output.stop(&error);
                                    return Err(error);
                                }
                                if !output.failed()
                                    && let Err(error) = execution.drain_output(index, output)
                                {
                                    output.stop(&error);
                                    return Err(error);
                                }
                            }
                        }
                        Ok(())
                    },
                )
                .err()
            };
        }
        // Both dispatch paths have settled all admitted groups before this
        // ownership transition. Resource permits were held through cleanup.
        if let Some(primary) = schedule_error.as_ref() {
            for index in owned_spools {
                if let Err(error) = spool::discard(&execution.output_path(index)) {
                    schedule_error = Some(Error::Io(format!(
                        "{primary}; temporary output cleanup also failed: {error}"
                    )));
                    break;
                }
            }
        }
        SettledSweep {
            activated: self,
            failures,
            schedule_error,
        }
    }
}

impl SettledSweep<'_> {
    pub fn finalize(self, output: &mut Presentation<'_>) -> Result<(), Error> {
        let Self {
            activated,
            failures,
            mut schedule_error,
        } = self;
        let ActivatedSweep {
            source,
            parent,
            plan,
            mut mode,
            options,
            results,
            warnings,
            reused: reused_guard,
            _output_lock,
            ..
        } = activated;
        let campaign_inputs = match &mode {
            ActiveMode::Campaigns(Some(checkpoint)) => Some(checkpoint),
            ActiveMode::Continued(guard) => Some(guard.inputs()),
            _ => None,
        };
        if let Some(checkpoint) = campaign_inputs
            && let Err(error) = checkpoint.seal_campaigns(&parent, &results, &plan)
        {
            let presentation = crate::runner::diagnostics::Warning::new(
                crate::runner::diagnostics::Stage::Group,
                Path::new(&source),
                &results.join(".checkpoint/campaign-completed"),
                &error,
            )
            .emit(options.json, output, &warnings);
            if let Err(error) = presentation {
                schedule_error = Some(match schedule_error {
                    Some(primary) => Error::Run(format!(
                        "{primary}; campaign continuation diagnostic also failed: {error}"
                    )),
                    None => error,
                });
            }
        }
        #[cfg(all(test, unix))]
        tests::before_input_finish(&results);
        let stability = match &mut mode {
            ActiveMode::Continued(guard) => guard.verify_stable(),
            ActiveMode::ReusedCampaigns(checkpoint) => checkpoint.verify_stable_borrowed(),
            ActiveMode::Campaigns(checkpoint) | ActiveMode::Fresh(checkpoint) => {
                checkpoint.take().map_or(Ok(()), |inputs| inputs.finish())
            }
            ActiveMode::Resumed(checkpoint) => checkpoint.finish_borrowed(),
        };
        if let Err(error) = stability {
            // A broken terminal cannot prevent final input settlement or
            // report generation; the original terminal error is retained.
            if !output.failed() {
                let presentation = crate::runner::diagnostics::Warning::new(
                    crate::runner::diagnostics::Stage::InputFinal,
                    Path::new(&source),
                    &results.join(".checkpoint/inputs.json"),
                    &error,
                )
                .emit(options.json, output, &warnings);
                if let Err(error) = presentation {
                    schedule_error = Some(match schedule_error {
                        Some(primary) => Error::Run(format!(
                            "{primary}; continuation diagnostic output also failed: {error}"
                        )),
                        None => error,
                    });
                }
            } else {
                // Keep the continuation stage visible without leaking the
                // arbitrary helper diagnostic or retrying a failed terminal.
                let diagnostic = "final input-stability seal unavailable; continuation diagnostic output suppressed after prior output failure";
                schedule_error = Some(match schedule_error {
                    Some(primary) => Error::Run(format!("{primary}; {diagnostic}")),
                    None => Error::Run(diagnostic.into()),
                });
            }
        }
        #[cfg(all(test, unix))]
        tests::before_comparison(Path::new(&source));
        let comparison_error = if options.json || output.failed() {
            compare::run_in_sweep(&source, &mut std::io::sink(), &_output_lock).err()
        } else if options.quiet_output {
            compare::run_links_in_sweep(&source, output, &_output_lock).err()
        } else {
            compare::run_in_sweep(&source, output, &_output_lock).err()
        };
        let primary = if failures.is_empty() {
            comparison_error.map_or(Ok(()), Err)
        } else {
            let failed = failures
                .iter()
                .map(|(_, configs)| configs.len())
                .sum::<usize>();
            let continued = options
                .continue_after_failures
                .map_or_else(String::new, |limit| {
                    if limit == usize::MAX {
                        " (continued after these failures)".into()
                    } else {
                        format!(" (continued with failure budget {limit})")
                    }
                });
            let mut details = if !options.quiet_output {
                format!(
                    "sweep completed with {failed} failed benchmark(s){continued}: {}",
                    failures
                        .iter()
                        .map(|(error, configs)| format!("{}: {error}", configs.join(", ")))
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            } else {
                format!(
                    "sweep completed with {failed} failed benchmark(s){continued}; rerun with --verbose for diagnostics"
                )
            };
            if let Some(error) = comparison_error {
                details.push_str(&format!("; comparison failed: {error}"));
            }
            Err(Error::Run(details))
        };
        // Keep the output lock in this final state through report publication.
        let outcome = crate::runner::diagnostics::preserve_primary(
            primary,
            schedule_error.map_or(Ok(()), Err),
        );
        drop(reused_guard);
        drop(mode);
        drop(_output_lock);
        outcome
    }
}
