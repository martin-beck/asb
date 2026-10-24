//! One operational decision over independent execution, evaluation and cleanup
//! facts. These producer types do not restrict historical report categories or
//! the workload-owned evaluation-kind extension surface.
use crate::workloads::outcome::EvaluationOutcome;
use serde::{Serialize, Serializer, ser::SerializeStruct};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RunStatus {
    EvaluationPending,
    Unscored,
    SmokePassed,
    Completed,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureKind {
    RunError,
    AgentFailure,
    WorkloadCheckFailed,
    GraderError,
    CollectorError,
    OutputError,
    ProviderCleanupError,
    RecordingError,
}

impl FailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunError => "run_error",
            Self::AgentFailure => "agent_failure",
            Self::WorkloadCheckFailed => "workload_check_failed",
            Self::GraderError => "grader_error",
            Self::CollectorError => "collector_error",
            Self::OutputError => "output_error",
            Self::ProviderCleanupError => "provider_cleanup_error",
            Self::RecordingError => "recording_error",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunFailure {
    pub kind: FailureKind,
    pub reason: String,
}

impl RunFailure {
    fn new(kind: FailureKind, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
        }
    }

    pub fn evaluation(evaluation: &EvaluationOutcome) -> Option<Self> {
        match evaluation {
            EvaluationOutcome::Completed {
                resolution: Some(false),
                reason,
                ..
            } => Some(Self::new(
                FailureKind::WorkloadCheckFailed,
                format!(
                    "workload check failed: {}",
                    reason.as_deref().unwrap_or("unresolved")
                ),
            )),
            EvaluationOutcome::Error { reason } => Some(Self::new(
                FailureKind::GraderError,
                format!("workload grader could not evaluate the result: {reason}"),
            )),
            _ => None,
        }
    }
}

pub(super) struct RunFacts<'a> {
    pub evaluation: &'a EvaluationOutcome,
    pub evaluation_kind: crate::workloads::KnownEvaluationKind,
    pub process_success: bool,
    pub agent_failure: Option<&'a str>,
    /// The runner could not retain a complete agent transcript. This is
    /// distinct from an execution or evaluator failure: a completed
    /// evaluator can still authoritatively resolve the workload.
    pub capture_incomplete: bool,
    pub run_error: Option<&'a str>,
    pub collector_error: Option<&'a str>,
    pub output_error: Option<&'a str>,
    pub provider_cleanup_error: Option<&'a str>,
    /// A normal workload/proxy cleanup error already contributes its original
    /// diagnostic to run_error. Keep confirmation explicit so unknown cleanup
    /// cannot silently become successful disposition even without that error.
    pub workload_cleanup_confirmed: bool,
    pub proxy_cleanup_confirmed: bool,
}

#[derive(Clone, Debug)]
pub(super) struct RunDisposition {
    pub status: RunStatus,
    pub failure: Option<RunFailure>,
    pub success: bool,
}

impl RunDisposition {
    /// Recording commits follow finalized execution observations but precede
    /// immutable publication. Keep grades/primary failures while failing the
    /// operational disposition if the recording cannot be committed.
    pub fn recording_failed(&mut self, reason: String) {
        self.status = RunStatus::Failed;
        self.success = false;
        self.failure
            .get_or_insert_with(|| RunFailure::new(FailureKind::RecordingError, reason));
    }

    pub fn from_facts(facts: RunFacts<'_>) -> Self {
        // A completed evaluator is the workload authority. A bounded
        // transcript capture overflow makes transcript-derived process
        // interpretation unavailable, but must not replace a confirmed grade.
        // Do not relax any other operational error, and do not apply this to
        // pending, unknown, or failed evaluation outcomes.
        let evaluator_authoritative_despite_capture = facts.capture_incomplete
            && matches!(
                facts.evaluation,
                EvaluationOutcome::Completed {
                    resolution: Some(_),
                    ..
                }
            );
        let failure = facts
            .run_error
            .map(|reason| RunFailure::new(FailureKind::RunError, reason))
            .or_else(|| {
                (!facts.workload_cleanup_confirmed || !facts.proxy_cleanup_confirmed).then(|| {
                    RunFailure::new(
                        FailureKind::RunError,
                        "owned-resource cleanup not confirmed",
                    )
                })
            })
            .or_else(|| {
                (!evaluator_authoritative_despite_capture)
                    .then_some(())
                    .and_then(|()| {
                        facts
                            .agent_failure
                            .map(|reason| RunFailure::new(FailureKind::AgentFailure, reason))
                    })
            })
            .or_else(|| {
                (!evaluator_authoritative_despite_capture && !facts.process_success).then(|| {
                    RunFailure::new(FailureKind::AgentFailure, "agent execution did not succeed")
                })
            })
            .or_else(|| RunFailure::evaluation(facts.evaluation))
            .or_else(|| {
                facts
                    .collector_error
                    .map(|reason| RunFailure::new(FailureKind::CollectorError, reason))
            })
            .or_else(|| {
                facts
                    .output_error
                    .map(|reason| RunFailure::new(FailureKind::OutputError, reason))
            })
            .or_else(|| {
                facts
                    .provider_cleanup_error
                    .map(|reason| RunFailure::new(FailureKind::ProviderCleanupError, reason))
            });
        let success = failure.is_none() && facts.evaluation.completed();
        let status = if failure.is_some() {
            RunStatus::Failed
        } else if matches!(facts.evaluation, EvaluationOutcome::Pending { .. }) {
            RunStatus::EvaluationPending
        } else if success
            && facts.evaluation_kind == crate::workloads::KnownEvaluationKind::InvalidBaseline
        {
            RunStatus::Unscored
        } else if success && !facts.evaluation_kind.resolution_eligible() {
            RunStatus::SmokePassed
        } else if success {
            RunStatus::Completed
        } else {
            RunStatus::Failed
        };
        Self {
            status,
            failure,
            success,
        }
    }
}

// Keep the existing log wire fields, including explicit null failure fields.
// Operational success is deliberately not a new field: the legacy public
// success_rate and measurement success remain independently grade-derived.
impl Serialize for RunDisposition {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut record = serializer.serialize_struct("RunDisposition", 3)?;
        record.serialize_field("status", &self.status)?;
        record.serialize_field("failure_kind", &self.failure.as_ref().map(|f| f.kind))?;
        record.serialize_field("failure_reason", &self.failure.as_ref().map(|f| &f.reason))?;
        record.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(evaluation: &EvaluationOutcome) -> RunFacts<'_> {
        RunFacts {
            evaluation,
            evaluation_kind: crate::workloads::KnownEvaluationKind::BundledFixture,
            process_success: true,
            agent_failure: None,
            capture_incomplete: false,
            run_error: None,
            collector_error: None,
            output_error: None,
            provider_cleanup_error: None,
            workload_cleanup_confirmed: true,
            proxy_cleanup_confirmed: true,
        }
    }

    #[test]
    fn recording_failure_preserves_evaluator_and_existing_primary_disposition() {
        for evaluation in [
            EvaluationOutcome::binary(true),
            EvaluationOutcome::binary(false),
            EvaluationOutcome::Pending {
                reason: "queued".into(),
            },
            EvaluationOutcome::Completed {
                resolution: None,
                rewards: [("accuracy".into(), 0.5)].into(),
                reason: None,
            },
        ] {
            let original = evaluation.clone();
            let mut disposition = RunDisposition::from_facts(facts(&evaluation));
            let primary = disposition.failure.clone();
            disposition.recording_failed("recording commit failed".into());
            assert_eq!(disposition.status, RunStatus::Failed);
            assert!(!disposition.success);
            if let Some(primary) = primary {
                assert_eq!(disposition.failure, Some(primary));
            } else {
                assert_eq!(
                    disposition.failure.as_ref().unwrap().kind,
                    FailureKind::RecordingError
                );
            }
            let first = disposition.failure.clone();
            disposition.recording_failed("later recording failure".into());
            assert_eq!(disposition.failure, first);
            let encoded = serde_json::to_value(&disposition).unwrap();
            assert_eq!(encoded["status"], "failed");
            assert_eq!(
                encoded["failure_kind"],
                disposition.failure.as_ref().unwrap().kind.as_str()
            );
            assert_eq!(evaluation, original);
        }
    }

    #[test]
    fn completed_evaluator_grade_outranks_only_capture_derived_process_failure() {
        let resolved_outcome = EvaluationOutcome::binary(true);
        let mut resolved = facts(&resolved_outcome);
        resolved.capture_incomplete = true;
        resolved.process_success = false;
        resolved.agent_failure = Some("agent output capture incomplete");
        let decision = RunDisposition::from_facts(resolved);
        assert_eq!(decision.status, RunStatus::Completed);
        assert!(decision.success);
        assert!(decision.failure.is_none());

        let unresolved_outcome = EvaluationOutcome::Pending {
            reason: "grade unavailable".into(),
        };
        let mut unresolved = facts(&unresolved_outcome);
        unresolved.capture_incomplete = true;
        unresolved.process_success = false;
        unresolved.agent_failure = Some("agent output capture incomplete");
        let decision = RunDisposition::from_facts(unresolved);
        assert_eq!(
            decision.failure.as_ref().map(|failure| failure.kind),
            Some(FailureKind::AgentFailure)
        );

        let failed_outcome = EvaluationOutcome::binary(false);
        let mut failed = facts(&failed_outcome);
        failed.capture_incomplete = true;
        failed.process_success = false;
        failed.agent_failure = Some("agent output capture incomplete");
        let decision = RunDisposition::from_facts(failed);
        assert_eq!(
            decision.failure.as_ref().map(|failure| failure.kind),
            Some(FailureKind::WorkloadCheckFailed)
        );
    }

    #[test]
    fn dispositions_preserve_evaluation_and_fail_operational_pending_states() {
        let evaluations = [
            EvaluationOutcome::binary(true),
            EvaluationOutcome::binary(false),
            EvaluationOutcome::Completed {
                resolution: None,
                rewards: [("accuracy".into(), 0.5)].into(),
                reason: None,
            },
            EvaluationOutcome::Pending {
                reason: "queued".into(),
            },
            EvaluationOutcome::Error {
                reason: "verifier failed".into(),
            },
        ];
        for evaluation in evaluations {
            let before = evaluation.clone();
            for fault in 0..9 {
                let mut input = facts(&evaluation);
                let operational = match fault {
                    0 => None,
                    1 => {
                        input.run_error = Some("run fault");
                        Some(FailureKind::RunError)
                    }
                    2 => {
                        input.process_success = false;
                        input.agent_failure = Some("client fault");
                        Some(FailureKind::AgentFailure)
                    }
                    3 => {
                        input.collector_error = Some("collector fault");
                        Some(FailureKind::CollectorError)
                    }
                    4 => {
                        input.output_error = Some("output fault");
                        Some(FailureKind::OutputError)
                    }
                    5 => {
                        input.provider_cleanup_error = Some("provider fault");
                        Some(FailureKind::ProviderCleanupError)
                    }
                    6 => {
                        input.workload_cleanup_confirmed = false;
                        input.run_error = Some("workload cleanup fault");
                        Some(FailureKind::RunError)
                    }
                    7 => {
                        input.proxy_cleanup_confirmed = false;
                        input.run_error = Some("proxy cleanup fault");
                        Some(FailureKind::RunError)
                    }
                    _ => {
                        input.process_success = false;
                        Some(FailureKind::AgentFailure)
                    }
                };
                let expected = if matches!(fault, 1 | 2 | 6 | 7 | 8) {
                    operational
                } else {
                    RunFailure::evaluation(&evaluation)
                        .map(|f| f.kind)
                        .or(operational)
                };
                let decision = RunDisposition::from_facts(input);
                assert_eq!(
                    decision.failure.as_ref().map(|f| f.kind),
                    expected,
                    "{evaluation:?} fault={fault}"
                );
                assert_eq!(
                    decision.success,
                    expected.is_none() && evaluation.completed()
                );
                assert_eq!(
                    decision.status,
                    if expected.is_some() {
                        RunStatus::Failed
                    } else if evaluation.completed() {
                        RunStatus::Completed
                    } else {
                        RunStatus::EvaluationPending
                    }
                );
                assert_eq!(
                    evaluation, before,
                    "disposition cannot mutate an evaluator outcome"
                );
            }
        }
    }

    #[test]
    fn primary_failure_precedence_and_ancillary_cleanup_remain_independent() {
        let evaluation = EvaluationOutcome::binary(false);
        let mut input = facts(&evaluation);
        input.run_error = Some("run");
        input.agent_failure = Some("agent");
        input.collector_error = Some("collector");
        input.output_error = Some("output");
        input.provider_cleanup_error = Some("provider");
        assert_eq!(
            RunDisposition::from_facts(input).failure.unwrap().kind,
            FailureKind::RunError
        );
        for (collector, output, expected) in [
            (true, true, FailureKind::CollectorError),
            (false, true, FailureKind::OutputError),
            (false, false, FailureKind::ProviderCleanupError),
        ] {
            let native = EvaluationOutcome::Completed {
                resolution: None,
                rewards: [("accuracy".into(), 0.5)].into(),
                reason: None,
            };
            let mut input = facts(&native);
            input.collector_error = collector.then_some("collector");
            input.output_error = output.then_some("output");
            input.provider_cleanup_error = Some("provider");
            assert_eq!(
                RunDisposition::from_facts(input).failure.unwrap().kind,
                expected
            );
        }
        let mut input = facts(&evaluation);
        input.workload_cleanup_confirmed = false;
        input.proxy_cleanup_confirmed = false;
        assert_eq!(
            RunDisposition::from_facts(input).failure.unwrap().kind,
            FailureKind::RunError
        );
        assert_eq!(evaluation.resolution(), Some(false));
    }

    #[test]
    fn statuses_and_failures_keep_exact_wire_strings_and_null_fields() {
        for (status, wire) in [
            (RunStatus::EvaluationPending, "evaluation-pending"),
            (RunStatus::Unscored, "unscored"),
            (RunStatus::SmokePassed, "smoke-passed"),
            (RunStatus::Completed, "completed"),
            (RunStatus::Failed, "failed"),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), wire);
        }
        for kind in [
            FailureKind::RunError,
            FailureKind::AgentFailure,
            FailureKind::WorkloadCheckFailed,
            FailureKind::GraderError,
            FailureKind::CollectorError,
            FailureKind::OutputError,
            FailureKind::ProviderCleanupError,
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), kind.as_str());
        }
        let evaluation = EvaluationOutcome::binary(true);
        let decision = RunDisposition::from_facts(facts(&evaluation));
        assert_eq!(
            serde_json::to_value(decision).unwrap(),
            serde_json::json!({"status":"completed","failure_kind":null,"failure_reason":null})
        );
        for (kind, expected) in [
            (
                crate::workloads::KnownEvaluationKind::InvalidBaseline,
                RunStatus::Unscored,
            ),
            (
                crate::workloads::KnownEvaluationKind::AdapterSmoke,
                RunStatus::SmokePassed,
            ),
            (
                crate::workloads::KnownEvaluationKind::Ungraded,
                RunStatus::SmokePassed,
            ),
        ] {
            let mut input = facts(&evaluation);
            input.evaluation_kind = kind;
            assert_eq!(RunDisposition::from_facts(input).status, expected);
        }
    }
}
