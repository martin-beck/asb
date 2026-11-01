//! Consuming transitions after execution. A task outcome is never a collector
//! result: once output exists, optional instrumentation cannot throw it away.
use super::*;

/// Intent publication is the admission boundary: no resources are allocated
/// and no project/provider discovery occurs until the observer accepts it.
pub(super) struct AdmittedAttempt<'a> {
    pub config: BenchmarkConfig,
    pub options: RunOptions<'a>,
    pub project_root: PathBuf,
    pub project_config: project::ProjectConfig,
}

impl<'a> AdmittedAttempt<'a> {
    pub fn admit(
        config_path: &Path,
        config: BenchmarkConfig,
        options: RunOptions<'a>,
    ) -> Result<Self, Error> {
        if options
            .execution_cohort
            .is_some_and(|cohort| !cohort.admits(options.replica.index, options.replica.total))
        {
            return Err(Error::Input(
                "dispatch cohort does not admit the selected replica".into(),
            ));
        }
        if let Some(observer) = options.on_completion {
            observer.starting(options.replica, options.execution_cohort)?;
        }
        let (project_root, project_config) = project::load(config_path)?;
        let project_root = project_root.canonicalize().map_err(|error| {
            Error::Input(format!(
                "cannot resolve benchmark project directory: {error}"
            ))
        })?;
        Ok(Self {
            config,
            options,
            project_root,
            project_config,
        })
    }
}

#[derive(Default)]
pub(super) struct AttemptIssues {
    pub run_error: Option<String>,
    pub capture_error: Option<String>,
    pub collector_errors: Vec<String>,
    pub output_error: Option<String>,
}

impl AttemptIssues {
    pub fn run_error(&mut self, message: String) {
        match self.run_error.as_mut() {
            Some(previous) => {
                previous.push_str("; ");
                previous.push_str(&message);
            }
            None => self.run_error = Some(message),
        }
    }

    pub fn event(&mut self, session: Option<&mut measurements::Session>, event: LifecycleEvent) {
        if let Some(session) = session
            && let Err(error) = session.event(event)
        {
            self.collector_errors.push(error);
        }
    }

    /// Unlike admission instrumentation, this must invoke the operation even
    /// if a begin/end/failure marker is unavailable. The operation owns the
    /// evaluator or cleanup side effect, not the collector.
    fn operation<T>(
        &mut self,
        mut session: Option<&mut measurements::Session>,
        phase: LifecyclePhase,
        operation: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        self.event(session.as_deref_mut(), LifecycleEvent::PhaseBegin(phase));
        let result = operation();
        self.event(session.as_deref_mut(), LifecycleEvent::PhaseEnd(phase));
        if result.is_err() {
            self.event(session, LifecycleEvent::PhaseFailed(phase));
        }
        result
    }

    pub fn collector_failure(&self) -> Option<String> {
        (!self.collector_errors.is_empty()).then(|| self.collector_errors.join("; "))
    }
}

pub(super) struct MeasuredOutput {
    pub observation: crate::process_termination::Observation,
    pub output: Output,
    pub elapsed_ms: u128,
    pub issues: AttemptIssues,
}

impl std::ops::Deref for MeasuredOutput {
    type Target = Output;
    fn deref(&self) -> &Output {
        &self.output
    }
}

pub(super) struct ExecutionEvidence {
    pub observation: crate::process_termination::Observation,
    pub artifact_owner: Option<agents::ArtifactOwner>,
    pub output: Output,
    pub capture_complete: bool,
    pub run_dir: PathBuf,
    pub guest: Option<agents::driver::GuestResult>,
    pub elapsed_ms: u128,
    pub runtime_output: Option<PathBuf>,
}

pub(super) struct AgentOutcome {
    pub final_response: Option<String>,
    pub failure: Option<String>,
}

impl ExecutionEvidence {
    pub(super) fn capture_complete(
        issues: &AttemptIssues,
        guest: Option<&agents::driver::GuestResult>,
    ) -> bool {
        issues.capture_error.is_none() && guest.is_none_or(durable_guest_presentation)
    }

    #[cfg(test)]
    pub fn agent_outcome(
        &self,
        agent: &str,
        workspace: &Path,
        artifact_response: Option<&str>,
    ) -> AgentOutcome {
        self.agent_outcome_with_artifacts(
            agent,
            workspace,
            artifact_response,
            &agents::FrozenArtifacts::default(),
        )
    }

    pub fn agent_outcome_with_artifacts(
        &self,
        agent: &str,
        workspace: &Path,
        artifact_response: Option<&str>,
        frozen: &agents::FrozenArtifacts,
    ) -> AgentOutcome {
        // Prefer the agent's structured final response when deciding whether a
        // zero-exit process succeeded. Tool output can legitimately contain a
        // transient failed command that the agent repairs before its final answer;
        // scanning that whole transcript would incorrectly fail the run.
        let child = &self.output;
        let guest = &self.guest;
        let final_response = match guest.as_ref() {
            Some(metadata) => match agents::find(agent) {
                Some(adapter) if adapter.uses_artifact_final_response() => metadata
                    .final_response
                    .as_ref()
                    .filter(|answer| {
                        metadata
                            .deferred_collection
                            .as_deref()
                            .and_then(|bytes| {
                                agents::FrozenArtifacts::from_deferred_collection_bytes(bytes).ok()
                            })
                            .is_some_and(|guest_frozen| {
                                adapter.validate_guest_artifact_answer(answer, &guest_frozen)
                            })
                    })
                    .cloned(),
                _ => metadata
                    .final_response
                    .clone()
                    .or_else(|| artifact_response.map(str::to_owned)),
            },
            None => {
                let adapter = agents::find(agent);
                if adapter.is_some_and(|adapter| adapter.uses_artifact_final_response()) {
                    adapter.and_then(|adapter| {
                        adapter.final_response_with_artifacts(
                            &String::from_utf8_lossy(&child.stdout),
                            frozen,
                            workspace,
                            self.capture_complete,
                        )
                    })
                } else {
                    artifact_response.map(str::to_owned).or_else(|| {
                        adapter.and_then(|adapter| {
                            let stdout = if self.capture_complete {
                                bounded(&child.stdout)
                            } else {
                                String::new()
                            };
                            adapter.final_response_with_artifacts(
                                &stdout,
                                frozen,
                                workspace,
                                self.capture_complete,
                            )
                        })
                    })
                }
            }
        };
        let adapter_failure = guest
            .as_ref()
            .and_then(|metadata| metadata.execution_failure.clone())
            .or_else(|| {
                // An isolated guest publishes its structured terminal result
                // independently of the bounded presentation stream.  Keep an
                // explicit truncation marker for continuation publication, but
                // do not replace a valid guest result with a guessed failure.
                // Native output has no such result, so it remains unavailable
                // for transcript-derived interpretation after truncation.
                guest.is_none().then_some(()).and_then(|()| {
                    (!self.capture_complete).then(|| {
                        "agent output capture incomplete; transcript interpretation unavailable"
                            .to_owned()
                    })
                })
            })
            .or_else(|| {
                if guest.is_none() {
                    agents::find(agent).and_then(|adapter| {
                        adapter.execution_failure(&bounded(&child.stdout), &bounded(&child.stderr))
                    })
                } else {
                    None
                }
            });
        let agent_failure = if adapter_failure.is_some() {
            adapter_failure
        } else {
            agents::find(agent)
                .and_then(|adapter| adapter.final_response_failure(final_response.as_deref()))
                .or_else(|| {
                    process_failure_reason(
                        &child.status,
                        &child.stdout,
                        &child.stderr,
                        final_response.as_deref(),
                    )
                })
        };
        AgentOutcome {
            final_response,
            failure: agent_failure,
        }
    }
}

/// A bounded isolated-guest presentation stream is not a failed capture when
/// the guest has independently sealed its terminal result.  Pipe failures and
/// unquiesced trees still remain completion blockers.
fn durable_guest_presentation(metadata: &agents::driver::GuestResult) -> bool {
    durable_guest_presentation_parts(
        metadata.transcript_truncated,
        metadata.tree_quiesced,
        metadata.artifact_error.is_none(),
        metadata.execution_failure.is_none(),
        metadata.exit_code,
    )
}

fn durable_guest_presentation_parts(
    truncated: bool,
    tree_quiesced: bool,
    artifact_settled: bool,
    execution_ok: bool,
    exit_code: Option<i32>,
) -> bool {
    !truncated || (tree_quiesced && artifact_settled && execution_ok && exit_code == Some(0))
}

/// Resources that must survive execution and, for adapter-owned services,
/// evaluation. On an aborted transition their existing adapter Drop contracts
/// cancel/preserve; only a completed evaluation reaches explicit finish.
pub(super) struct PreparedAttempt {
    pub workload: Box<dyn workloads::WorkloadSession>,
    pub measurements: Option<measurements::Session>,
    pub tape: Option<tape::Session>,
    pub capsule_record: Option<agents::capsule::Capture>,
    pub capsule_replay: Option<agents::capsule::Replay>,
    /// Remote guests capture their own actual workspace. These paths remain
    /// runner-private custody locations, not guest request fields.
    #[cfg_attr(test, allow(dead_code))]
    pub guest_capsule_record: Option<std::path::PathBuf>,
    #[cfg_attr(test, allow(dead_code))]
    pub guest_capsule_replay: Option<std::path::PathBuf>,
}

pub(super) struct ExecutedAttempt {
    prepared: PreparedAttempt,
    pub evidence: ExecutionEvidence,
    issues: AttemptIssues,
}

impl PreparedAttempt {
    pub fn executed(self, evidence: ExecutionEvidence, issues: AttemptIssues) -> ExecutedAttempt {
        ExecutedAttempt {
            prepared: self,
            evidence,
            issues,
        }
    }
}

pub(super) struct SettledAttempt {
    prepared: PreparedAttempt,
    pub evidence: ExecutionEvidence,
    pub proxy_evidence: crate::proxy::Evidence,
    pub cassette_usage_verified: bool,
    pub issues: AttemptIssues,
}

impl ExecutedAttempt {
    pub fn settle(mut self, cassette: Option<&(tape::Mode, PathBuf)>) -> SettledAttempt {
        let mut proxy_validated = false;
        let proxy_evidence = match self.prepared.tape.take() {
            Some(session) => {
                let completed = session.finish();
                proxy_validated = completed.result.is_ok();
                if let Err(error) = completed.result {
                    self.issues.run_error(error.to_string());
                }
                completed.evidence
            }
            None => crate::proxy::Evidence {
                captures: Vec::new(),
                output_files: Vec::new(),
                cleanup_confirmed: true,
            },
        };
        // A partially served replay does not establish usage of its whole tape.
        let cassette_usage_verified =
            !matches!(cassette, Some((tape::Mode::Replay, _))) || proxy_validated;
        if let Some((tape::Mode::Record, cassette)) = cassette {
            match std::fs::metadata(cassette) {
                Ok(metadata) if metadata.len() > 0 => {}
                Ok(_) => self.issues.run_error(format!("record created an empty cassette {}; the agent made no provider request; verify its provider base URL and credentials", cassette.display())),
                Err(error) => self.issues.run_error(format!("record completed without creating cassette {}; verify the provider endpoint and proxy adapter installation: {error}", cassette.display())),
            }
        }
        SettledAttempt {
            prepared: self.prepared,
            evidence: self.evidence,
            proxy_evidence,
            cassette_usage_verified,
            issues: self.issues,
        }
    }
}

impl SettledAttempt {
    /// Finalize the requested native capsule before evaluator and workload
    /// cleanup can alter the agent's workspace. Capture publishes immutable
    /// effects; replay applies only the recorded declared effects.
    pub fn finish_native_capsule(
        &mut self,
        workspace: &std::path::Path,
    ) -> Result<Option<PathBuf>, Error> {
        if let Some(capture) = self.prepared.capsule_record.take() {
            return capture.finish(workspace).map(Some);
        }
        self.prepared
            .capsule_replay
            .take()
            .map(|replay| replay.finish(workspace).map(|()| None))
            .transpose()
            .map(Option::flatten)
    }

    /// Retain an opaque guest-captured capsule at the runner-selected path.
    /// The receipt was verified by workload transport before this point; this
    /// function deliberately does not parse an adapter's tool protocol.
    pub fn retain_guest_capsule(&mut self) -> Result<(), Error> {
        let Some(destination) = self.prepared.guest_capsule_record.take() else {
            return Ok(());
        };
        let artifacts = &mut self
            .evidence
            .guest
            .as_mut()
            .ok_or_else(|| Error::Run("guest native capsule export has no guest result".into()))?
            .sealed_artifacts;
        let index = artifacts
            .iter()
            .position(|artifact| {
                artifact.kind == crate::agents::driver::NATIVE_WORKSPACE_CAPSULE_ARTIFACT
            })
            .ok_or_else(|| Error::Run("guest native capsule export is missing".into()))?;
        let bytes = artifacts.remove(index).bytes;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(Error::Run(
                "guest native capsule export exceeds its bound".into(),
            ));
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .map_err(crate::io_error)?;
        use std::io::Write;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(crate::io_error)
    }

    pub fn native_capsule_failure(&mut self, error: Error) {
        self.issues
            .run_error(format!("native capsule finalization failed: {error}"));
    }
}

pub(super) struct EvaluationOutcome {
    pub evaluation: workloads::outcome::EvaluationOutcome,
}

impl EvaluationOutcome {
    pub fn success(&self) -> Option<bool> {
        self.evaluation.resolution()
    }
}

pub(super) struct EvaluatedAttempt {
    settled: SettledAttempt,
    outcome: EvaluationOutcome,
    evaluator_evidence: Vec<PathBuf>,
}

impl SettledAttempt {
    #[cfg(test)]
    pub fn evaluate(self, response: &str) -> EvaluatedAttempt {
        let policy = self.prepared.workload.metric_policy();
        self.evaluate_with_policy(response, &policy)
    }

    pub fn evaluate_with_policy(
        mut self,
        response: &str,
        policy: &workloads::outcome::MetricPolicy,
    ) -> EvaluatedAttempt {
        use workloads::outcome::EvaluationOutcome as Evaluation;
        let mut evaluation = self
            .issues
            .operation(
                self.prepared.measurements.as_mut(),
                LifecyclePhase::WorkloadEvaluation,
                || Ok(self.prepared.workload.evaluate_outcome(response)),
            )
            .unwrap_or_else(|reason| Evaluation::Error { reason });
        if let Err(reason) = policy.validate_evaluation(&evaluation) {
            evaluation = Evaluation::Error { reason };
        }
        let evaluator_evidence = self.prepared.workload.checkpoint_evidence();
        if policy != &workloads::outcome::MetricPolicy::default()
            && evaluation.completed()
            && evaluator_evidence.is_empty()
        {
            evaluation = Evaluation::Error {
                reason: "native reward evaluator supplied no evidence artifacts".into(),
            };
        }
        if matches!(
            evaluation,
            Evaluation::Error { .. }
                | Evaluation::Completed {
                    resolution: Some(false),
                    ..
                }
        ) {
            self.issues.event(
                self.prepared.measurements.as_mut(),
                LifecycleEvent::PhaseFailed(LifecyclePhase::WorkloadEvaluation),
            );
        }
        EvaluatedAttempt {
            settled: self,
            outcome: EvaluationOutcome { evaluation },
            evaluator_evidence,
        }
    }
}

pub(super) struct FinalizedAttempt {
    pub workload: Box<dyn workloads::WorkloadSession>,
    pub measurements: Option<measurements::Session>,
    pub evidence: ExecutionEvidence,
    pub proxy_evidence: crate::proxy::Evidence,
    pub cassette_usage_verified: bool,
    pub outcome: EvaluationOutcome,
    pub evaluator_evidence: Vec<PathBuf>,
    pub cleanup_confirmed: bool,
    pub resource_disposition: Option<workloads::ResourceDisposition>,
    pub issues: AttemptIssues,
}

impl EvaluatedAttempt {
    pub fn finish(mut self) -> FinalizedAttempt {
        let cleanup = self.settled.issues.operation(
            self.settled.prepared.measurements.as_mut(),
            LifecyclePhase::WorkloadCleanup,
            || self.settled.prepared.workload.finish(),
        );
        let cleanup_confirmed = cleanup.is_ok();
        let resource_disposition = cleanup.as_ref().ok().map(|settled| settled.disposition);
        let diagnostic = match cleanup {
            Ok(settled) => settled.diagnostic,
            Err(error) => Some(error),
        };
        if let Some(error) = diagnostic {
            if cleanup_confirmed {
                self.settled.issues.event(
                    self.settled.prepared.measurements.as_mut(),
                    LifecycleEvent::PhaseFailed(LifecyclePhase::WorkloadCleanup),
                );
            }
            self.settled
                .issues
                .run_error(format!("workload cleanup failed: {error}"));
        }
        FinalizedAttempt {
            workload: self.settled.prepared.workload,
            measurements: self.settled.prepared.measurements,
            evidence: self.settled.evidence,
            proxy_evidence: self.settled.proxy_evidence,
            cassette_usage_verified: self.settled.cassette_usage_verified,
            outcome: self.outcome,
            evaluator_evidence: self.evaluator_evidence,
            cleanup_confirmed,
            resource_disposition,
            issues: self.settled.issues,
        }
    }
}

#[cfg(all(test, unix))]
#[path = "../../tests/unit/runner_phases.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "../../tests/unit/runner_entry_support.rs"]
pub(super) mod entry_test_support;

/// Log publication and measurement publication are independent post-grade
/// obligations. Failure of either prevents a completion receipt, not grading.
pub(super) enum LogPublication {
    Confirmed,
    Failed(Error),
}
impl LogPublication {
    pub fn publish(config_path: &Path, configured_path: &str, result: &RunResult<'_>) -> Self {
        match write_log(config_path, configured_path, result) {
            Ok(()) => Self::Confirmed,
            Err(error) => Self::Failed(error),
        }
    }
    pub fn confirmed(&self) -> bool {
        matches!(self, Self::Confirmed)
    }
    pub fn with_artifact_evidence(self, paths: &[String]) -> Self {
        match self {
            Self::Failed(error) if !paths.is_empty() => Self::Failed(Error::Run(format!(
                "{error}; retained frozen agent evidence (relative to private attempt runtime parent): {}",
                paths.join(", ")
            ))),
            other => other,
        }
    }
    pub fn into_error(self) -> Option<Error> {
        match self {
            Self::Confirmed => None,
            Self::Failed(error) => Some(Error::Run(format!(
                "run log publication failed after task evaluation: {error}; inspect the configured log directory and saved measurements; do not repeat the agent to repair reporting"
            ))),
        }
    }
}

pub(super) fn final_outcome(
    primary: Option<RunFailure>,
    log: LogPublication,
    measurements: MeasurementCompletion,
) -> Result<(), Error> {
    let mut errors = Vec::new();
    if let Some(failure) = primary {
        errors.push(Error::Run(failure.reason));
    }
    errors.extend(log.into_error());
    errors.extend(measurements.into_error());
    let mut errors = errors.into_iter();
    let Some(primary) = errors.next() else {
        return Ok(());
    };
    let secondary: Vec<_> = errors.collect();
    if secondary.is_empty() {
        Err(primary)
    } else {
        Err(Error::Run(format!(
            "{primary}; {}",
            secondary
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )))
    }
}

pub(super) enum MeasurementCompletion {
    Confirmed,
    Incomplete(Vec<String>),
}
impl MeasurementCompletion {
    pub fn confirmed(&self) -> bool {
        matches!(self, Self::Confirmed)
    }
    pub fn with_artifact_evidence(self, paths: &[String]) -> Self {
        match self {
            Self::Incomplete(mut errors) if !paths.is_empty() => {
                errors.push(format!("retained frozen agent evidence (relative to private attempt runtime parent): {}", paths.join(", ")));
                Self::Incomplete(errors)
            }
            other => other,
        }
    }
    pub fn into_error(self) -> Option<Error> {
        match self {
            Self::Confirmed => None,
            Self::Incomplete(errors) => Some(Error::Run(format!(
                "measurement finalization failed after task evaluation: {}; inspect saved task/measurement evidence; do not repeat the agent to repair reporting",
                errors.join("; ")
            ))),
        }
    }
}

/// This finalization cannot change the immutable result. Incomplete publication
/// prohibits a completion receipt, but must not hide a freshly computed grade.
pub(super) fn finish_measurements(
    session: Option<measurements::Session>,
    result: &RunResult<'_>,
) -> MeasurementCompletion {
    let Some(mut session) = session else {
        return MeasurementCompletion::Confirmed;
    };
    let mut issues = AttemptIssues::default();
    if !result.disposition.success {
        issues.event(Some(&mut session), LifecycleEvent::RunFailed);
    }
    issues.event(Some(&mut session), LifecycleEvent::RunFinishing);
    let measured = measurements::RunMeasurement {
        planning: Some(&result.planning),
        effective_budgets: Some(result.effective_budgets),
        execution: result.execution.as_ref(),
        recording: result.recording.as_ref(),
        observation: result.observation,
        workload_resource_disposition: result.workload_resource_disposition,
        workload_artifacts: &result.workload_artifacts,
        execution_plan_fingerprint: result
            .execution_plan_fingerprint
            .as_ref()
            .map(|fingerprint| fingerprint.as_str()),
        module: result.module.as_ref(),
        benchmark_outcome: result.benchmark_outcome.as_ref(),
        replica_index: result.replica_index,
        execution_cohort: result.execution_cohort,
        schema_version: 2,
        run_id: format!("{}-{}", result.workload, result.elapsed_ms),
        status: result.disposition.status,
        success: result.success_rate == Some(1.0),
        process_success: result.process_success,
        workload_success: result.workload_success,
        failure_kind: result.disposition.failure.as_ref().map(|f| f.kind),
        failure_reason: result
            .disposition
            .failure
            .as_ref()
            .map(|f| f.reason.as_str()),
        workload_evaluation: result.workload_evaluation,
        workload_baseline_passed: result.workload_baseline_passed,
        workload_baseline: Some(&result.workload_baseline),
        workload_failure: result.workload_failure.as_ref(),
        adapter_smoke_success: result.adapter_smoke_success,
        success_rate: result.success_rate,
        agent: result.agent,
        workload: result.workload,
        task: Some(&result.task),
        provider: result.provider,
        model: result.model,
        elapsed_ms: result.elapsed_ms,
        exit_code: result.exit_code,
        token_usage: result.token_usage.clone(),
        steps: result.steps,
        tool_call_count: result
            .tool_calls_available
            .then_some(result.tool_calls.len()),
        tool_calls: &result.tool_calls,
        started_unix_ms: session.started_unix_ms(),
    };

    if let Err(error) = session.finish(measured) {
        issues.collector_errors.push(error);
    }
    if issues.collector_errors.is_empty() {
        MeasurementCompletion::Confirmed
    } else {
        MeasurementCompletion::Incomplete(issues.collector_errors)
    }
}
