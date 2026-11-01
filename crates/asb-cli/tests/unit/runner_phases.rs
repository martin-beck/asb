//! Offline evidence/grade/resource transition contracts, not SDK qualification.
use super::*;
use crate::test_support as support;
use std::cell::RefCell;
use std::rc::Rc;

#[path = "runner_recording_capacity.rs"]
mod recording_capacity;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/runner-phase-fixtures")
            .join(workloads::TaskIdentity::local("phases", None).attempt_id);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = crate::fixture_writer::cleanup(&self.0);
        #[cfg(not(unix))]
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Workload {
    invocation: workloads::session::TaskInvocation,
    operations: Rc<RefCell<Vec<&'static str>>>,
    grade: u8,
    cleanup_error: bool,
    finished: bool,
}
impl workloads::WorkloadSession for Workload {
    fn invocation(&self) -> &workloads::session::TaskInvocation {
        &self.invocation
    }
    fn evaluation_kind(&self) -> workloads::KnownEvaluationKind {
        workloads::KnownEvaluationKind::UpstreamHarness
    }
    fn baseline_passed(&self) -> Option<bool> {
        Some(false)
    }
    fn evaluate(&mut self, response: &str) -> Result<workloads::Evaluation, String> {
        assert_eq!(response, "submission");
        self.operations.borrow_mut().push("evaluate");
        match self.grade {
            0 => Ok(workloads::Evaluation::Passed),
            1 => Ok(workloads::Evaluation::Failed(
                "official unresolved task".into(),
            )),
            _ => Err("official verifier unavailable".into()),
        }
    }
    fn metric_policy(&self) -> workloads::outcome::MetricPolicy {
        use workloads::outcome::*;
        if self.grade < 3 {
            return MetricPolicy::default();
        }
        MetricPolicy {
            id: "accuracy".into(),
            reward: "accuracy".into(),
            rewards: std::collections::BTreeMap::from([(
                "accuracy".into(),
                RewardSpec {
                    unit: "ratio".into(),
                    range: Some([0.0, 1.0]),
                },
            )]),
            ..Default::default()
        }
    }
    fn checkpoint_evidence(&self) -> Vec<PathBuf> {
        if self.grade >= 3 {
            vec![self.invocation.workspace.join("grade.json")]
        } else {
            vec![]
        }
    }
    fn evaluate_outcome(&mut self, response: &str) -> workloads::outcome::EvaluationOutcome {
        use workloads::outcome::EvaluationOutcome as Evaluation;
        if self.grade >= 3 {
            assert_eq!(response, "submission");
            self.operations.borrow_mut().push("evaluate");
            return match self.grade {
                3 => Evaluation::Completed {
                    resolution: None,
                    rewards: std::collections::BTreeMap::from([("accuracy".into(), 0.5)]),
                    reason: None,
                },
                4 => Evaluation::Pending {
                    reason: "queued".into(),
                },
                _ => Evaluation::Error {
                    reason: "native verifier unavailable".into(),
                },
            };
        }
        match self.evaluate(response) {
            Ok(workloads::Evaluation::Passed) => Evaluation::binary(true),
            Ok(workloads::Evaluation::Failed(reason)) => Evaluation::Completed {
                resolution: Some(false),
                rewards: std::collections::BTreeMap::from([("resolution".into(), 0.0)]),
                reason: Some(reason),
            },
            Err(reason) => Evaluation::Error { reason },
        }
    }
    fn finish(&mut self) -> Result<workloads::session::ResourceSettlement, String> {
        self.operations.borrow_mut().push(if self.grade == 2 {
            "preserve"
        } else {
            "finish"
        });
        self.finished = true;
        if self.cleanup_error {
            Err("owned cleanup unavailable".into())
        } else {
            Ok(workloads::session::ResourceSettlement {
                disposition: if self.grade >= 2 {
                    workloads::ResourceDisposition::StoppedPreserved
                } else {
                    workloads::ResourceDisposition::Removed
                },
                diagnostic: (self.grade == 6)
                    .then(|| "normal removal failed; owned resources stopped".into()),
            })
        }
    }
}
impl Drop for Workload {
    fn drop(&mut self) {
        if !self.finished {
            self.operations.borrow_mut().push("cancel-preserve");
        }
    }
}
fn prepared(
    fixture: &Fixture,
    grade: u8,
    cleanup_error: bool,
    measurements: Option<measurements::Session>,
) -> (PreparedAttempt, Rc<RefCell<Vec<&'static str>>>) {
    if grade >= 3 {
        std::fs::write(fixture.0.join("grade.json"), "native evaluator evidence").unwrap();
    }
    let operations = Rc::new(RefCell::new(Vec::new()));
    (
        PreparedAttempt {
            workload: Box::new(Workload {
                invocation: workloads::session::TaskInvocation {
                    identity: workloads::TaskIdentity::local("fixture", None),
                    input: "fixture".into(),
                    workspace: fixture.0.clone(),
                },
                operations: operations.clone(),
                grade,
                cleanup_error,
                finished: false,
            }),
            measurements,
            tape: None,
            capsule_record: None,
            capsule_replay: None,
            guest_capsule_record: None,
            guest_capsule_replay: None,
        },
        operations,
    )
}
fn evidence(exit: i32, guest: bool) -> ExecutionEvidence {
    use std::os::unix::process::ExitStatusExt;
    ExecutionEvidence {
        observation: Default::default(),
        artifact_owner: None,
        capture_complete: true,
        output: Output {
            status: std::process::ExitStatus::from_raw(exit << 8),
            stdout: b"submission".to_vec(),
            stderr: b"diagnostic".to_vec(),
        },
        run_dir: PathBuf::new(),
        runtime_output: None,
        elapsed_ms: 17,
        guest: guest.then(|| {
            serde_json::from_value(serde_json::json!({
                "schema_version":1,"exit_code":exit,"final_response":"submission",
                "token_usage":{},"steps":3,"execution_failure":null,"tree_quiesced":true
            }))
            .unwrap()
        }),
    }
}

#[test]
fn native_guest_grade_and_cleanup_matrix_preserves_independent_facts() {
    // Each completed process, even an unsuccessful one, reaches the evaluator
    // exactly once. Instrumentation and cleanup never rewrite its grade.
    for guest in [false, true] {
        for exit in [0, 7, 124] {
            for grade in [0, 1, 2] {
                for cleanup_error in [false, true] {
                    let fixture = Fixture::new();
                    let (prepared, operations) = prepared(&fixture, grade, cleanup_error, None);
                    let mut issues = AttemptIssues::default();
                    issues
                        .collector_errors
                        .push("capture shutdown failed".into());
                    let execution = evidence(exit, guest);
                    let agent = execution.agent_outcome("fixture", &fixture.0, None);
                    assert_eq!(agent.failure.is_none(), exit == 0);
                    let final_state = prepared
                        .executed(execution, issues)
                        .settle(None)
                        .evaluate("submission")
                        .finish();
                    assert_eq!(
                        final_state.outcome.success(),
                        match grade {
                            0 => Some(true),
                            1 => Some(false),
                            _ => None,
                        }
                    );
                    assert_eq!(final_state.cleanup_confirmed, !cleanup_error);
                    assert_eq!(
                        final_state.resource_disposition,
                        (!cleanup_error).then_some(if grade == 2 {
                            workloads::ResourceDisposition::StoppedPreserved
                        } else {
                            workloads::ResourceDisposition::Removed
                        })
                    );
                    assert_eq!(final_state.evidence.output.status.code(), Some(exit));
                    assert_eq!(final_state.evidence.output.stdout, b"submission");
                    assert_eq!(final_state.evidence.guest.is_some(), guest);
                    assert_eq!(
                        final_state.issues.collector_failure().as_deref(),
                        Some("capture shutdown failed")
                    );
                    assert_eq!(final_state.issues.run_error.is_some(), cleanup_error);
                    assert_eq!(
                        operations.borrow().as_slice(),
                        ["evaluate", if grade == 2 { "preserve" } else { "finish" }]
                    );
                    drop(final_state);
                    assert_eq!(
                        operations.borrow().len(),
                        2,
                        "completed attempts cannot cancel/rerun on drop"
                    );
                }
            }
        }
    }
}

#[test]
fn required_prelaunch_lifecycle_errors_admit_no_client_process() {
    for event in [
        LifecycleEvent::MeasurementsReady,
        LifecycleEvent::AgentSpawn,
        LifecycleEvent::AgentExecuteBegin,
    ] {
        let fixture = Fixture::new();
        let marker = fixture.0.join("executed");
        let mut session =
            measurements::Session::start_with_collectors(&fixture.0.join("measure"), &[], None)
                .unwrap();
        session.test_event_failure = Some(event);
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf executed > \"$1\"", "fixture"])
            .arg(&marker)
            .env_clear()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let target = crate::launch::TargetLaunch::direct(&command);
        let error = run_measured_agent(
            &mut command,
            &target,
            None,
            Some(&mut session),
            false,
            &mut Vec::new(),
        )
        .err()
        .unwrap();
        assert!(
            error
                .to_string()
                .contains("fixture collector event failure")
        );
        assert!(
            !marker.exists(),
            "required prelaunch failure must remain fail-closed"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn changed_gate_carrier_or_target_framing_is_rejected_before_custody_or_execution() {
    use std::os::unix::fs::PermissionsExt;
    for variant in ["carrier", "framing", "short", "target", "adapter-prefix"] {
        let fixture = Fixture::new();
        let marker = fixture.0.join("must-not-run");
        let gate_program = fixture.0.join("fixture-gate");
        std::fs::write(
            &gate_program,
            "#!/bin/sh\nprintf executed > \"$ASB_REPLACEMENT_MARKER\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&gate_program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut launch = crate::launch::ProcessLaunch::new("/bin/true");
        launch.arg("immutable-adapter-prefix");
        let (mut command, route) = launch.native(Some(&gate_program));
        assert_eq!(command.get_program(), gate_program.as_os_str());
        command = if variant == "carrier" {
            let mut replacement = Command::new("/bin/sh");
            replacement.args(["-c", "printf executed > \"$ASB_REPLACEMENT_MARKER\""]);
            replacement
        } else {
            let mut replacement = Command::new(&gate_program);
            match variant {
                "framing" => {
                    replacement.args(["wrong-framing", "ignored"]);
                }
                "target" => {
                    replacement.args(["__asb-gate", crate::launch::FORMAT, "0", "/bin/false"]);
                }
                "adapter-prefix" => {
                    replacement.args([
                        "__asb-gate",
                        crate::launch::FORMAT,
                        "0",
                        "/bin/true",
                        "changed-adapter-prefix",
                    ]);
                }
                "short" => {}
                _ => unreachable!(),
            }
            replacement
        };
        command
            .env("ASB_REPLACEMENT_MARKER", &marker)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let target = route.target(&command);
        let scope = crate::process_cleanup::test_support::Scope::new();
        let error = run_measured_agent(
            &mut command,
            &target,
            Some(route),
            None,
            false,
            &mut Vec::new(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            error.contains("cannot validate ASB gate launch")
                && error.contains("without replacing it"),
            "{variant}: {error}"
        );
        assert_eq!(
            scope.count(),
            0,
            "invalid route must not establish a reaper"
        );
        assert!(!marker.exists(), "{variant}: replacement command executed");
    }
}

#[test]
fn confirmed_preservation_keeps_diagnostic_without_fabricating_a_grade() {
    let fixture = Fixture::new();
    let (prepared, operations) = prepared(&fixture, 6, false, None);
    let finalized = prepared
        .executed(evidence(7, false), AttemptIssues::default())
        .settle(None)
        .evaluate("submission")
        .finish();
    assert!(finalized.cleanup_confirmed);
    assert_eq!(
        finalized.resource_disposition,
        Some(workloads::ResourceDisposition::StoppedPreserved)
    );
    assert_eq!(finalized.outcome.success(), None);
    assert!(
        finalized
            .issues
            .run_error
            .as_deref()
            .unwrap()
            .contains("normal removal failed")
    );
    assert_eq!(operations.borrow().as_slice(), ["evaluate", "finish"]);
}

#[cfg(target_os = "linux")]
#[test]
fn native_launch_waits_for_siblings_and_refuses_an_unverifiable_sibling_gate() {
    use support::IsolatedEnvironment;
    if std::env::var_os("ASB_NATIVE_LAUNCH_TEST_CHILD").is_none() {
        let executable = std::env::current_exe().unwrap();
        let runtime = executable.parent().unwrap().parent().unwrap().join("asb");
        let child = Command::new(&executable)
            .args(["--exact", "runner::phases::tests::native_launch_waits_for_siblings_and_refuses_an_unverifiable_sibling_gate", "--nocapture"])
            .isolated_env()
            .env("ASB_NATIVE_LAUNCH_TEST_CHILD", "1")
            .env("ASB_RUNTIME_EXECUTABLE", runtime)
            .output().unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stdout)
        );
        return;
    }
    use crate::workloads::session::LaunchReadiness;
    use std::sync::mpsc;
    struct Announce<'a> {
        admission: super::super::batch::LaunchAdmission<'a>,
        arrived: mpsc::Sender<()>,
    }
    impl LaunchReadiness for Announce<'_> {
        fn ready(&self) -> Result<(), String> {
            self.arrived.send(()).unwrap();
            self.admission.ready()
        }
        fn abort(&self) {
            self.admission.abort();
        }
        fn remaining(&self) -> Result<Duration, String> {
            self.admission.remaining()
        }
    }
    fn material() -> crate::cassette::Compatibility {
        crate::cassette::Compatibility {
            agent: "fixture".into(),
            provider: "fixture".into(),
            model: "fixture".into(),
            workload: "fixture".into(),
            task: "fixture".into(),
            dataset_sha256: None,
            prompt_sha256: "a".repeat(64),
            material_sha256: "b".repeat(64),
            protocol_sha256: None,
        }
    }
    fn run(
        marker: &Path,
        launch: &dyn LaunchReadiness,
        verifiable: bool,
    ) -> Result<MeasuredOutput, Error> {
        let gate = crate::process_gate::executable().unwrap();
        let (mut command, route) = crate::launch::ProcessLaunch::new("/bin/sh")
            .native(verifiable.then_some(gate.as_path()));
        command
            .args([
                "-c",
                "printf started > \"$1\"",
                "fixture",
                marker.to_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let target = route.target(&command);
        run_measured_agent_with_launch(
            &mut command,
            &target,
            Some(route),
            None,
            false,
            &mut Vec::new(),
            NativeLaunch {
                readiness: Some(launch),
                runtime: MAX_AGENT_RUNTIME,
            },
        )
    }
    for verifiable in [false, true] {
        let fixture = Fixture::new();
        let gate = super::super::batch::Gate::deferred(2, Duration::from_secs(5)).unwrap();
        let first_marker = fixture.0.join("first");
        let second_marker = fixture.0.join("second");
        std::thread::scope(|scope| {
            let (tx, rx) = mpsc::channel();
            let first_gate = &gate;
            let first_marker = &first_marker;
            let first = scope.spawn(move || {
                let admission = Announce {
                    admission: super::super::batch::LaunchAdmission::new(
                        first_gate,
                        replica::Identity { index: 1, total: 2 },
                        material(),
                    )
                    .unwrap(),
                    arrived: tx,
                };
                run(first_marker, &admission, true)
            });
            rx.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(
                !first_marker.exists(),
                "ready replica executed before its sibling was prepared"
            );
            let second = super::super::batch::LaunchAdmission::new(
                &gate,
                replica::Identity { index: 2, total: 2 },
                material(),
            )
            .unwrap();
            let result = run(&second_marker, &second, verifiable);
            drop(second);
            assert_eq!(result.is_ok(), verifiable);
            assert_eq!(first.join().unwrap().is_ok(), verifiable);
        });
        assert_eq!(first_marker.exists(), verifiable);
        assert_eq!(second_marker.exists(), verifiable);
    }
}

#[test]
fn spawn_and_wait_errors_remain_primary_when_failure_markers_also_fail() {
    for spawn_failure in [false, true] {
        let fixture = Fixture::new();
        let mut session =
            measurements::Session::start_with_collectors(&fixture.0.join("measure"), &[], None)
                .unwrap();
        session.test_event_failure = Some(LifecycleEvent::AgentExecuteEnd);
        let mut command = if spawn_failure {
            Command::new(fixture.0.join("missing-client"))
        } else {
            Command::new("/bin/true")
        };
        // Deliberately leave pipes absent in the wait-failure case.
        let mut target = crate::launch::TargetLaunch::direct(&command);
        target.program = "fixture".into();
        let error = run_measured_agent(
            &mut command,
            &target,
            None,
            Some(&mut session),
            false,
            &mut Vec::new(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            error.contains(if spawn_failure {
                "could not start fixture"
            } else {
                "agent stdout was not piped"
            }),
            "{error}"
        );
        assert!(error.contains("fixture collector event failure"));
    }
}

#[test]
fn native_agent_outcome_uses_typed_terminal_error_and_ignores_task_text() {
    let mut execution = evidence(0, false);
    execution.output.stdout =
        br#"{"type":"result","subtype":"success","is_error":true,"result":"private error"}"#
            .to_vec();
    let failed = execution.agent_outcome("claude-code", Path::new("."), None);
    let reason = failed.failure.expect("typed error must fail");
    assert!(reason.contains("terminal error"));
    assert!(!reason.contains("private error"));
    execution.output.stdout =
        br#"{"type":"result","subtype":"success","is_error":false,"result":"litellm.NotFoundError example"}"#.to_vec();
    assert!(
        execution
            .agent_outcome("claude-code", Path::new("."), None)
            .failure
            .is_none()
    );
    execution.output.stdout =
        br#"{"type":"turn_end","message":{"role":"assistant","stopReason":"error"}}"#.to_vec();
    assert!(
        execution
            .agent_outcome("pi", Path::new("."), None)
            .failure
            .is_none()
    );
    let failed_pi = evidence(1, false).agent_outcome("pi", Path::new("."), None);
    assert!(
        failed_pi
            .failure
            .as_deref()
            .unwrap()
            .contains("exit status: 1")
    );
}

#[test]
fn guest_uses_frozen_adapter_answer_when_controller_has_none() {
    let mut execution = evidence(0, true);
    execution.guest.as_mut().unwrap().final_response = None;
    let outcome = execution.agent_outcome("codex", Path::new("."), Some("verified final answer"));
    assert_eq!(
        outcome.final_response.as_deref(),
        Some("verified final answer")
    );
    assert!(outcome.failure.is_none());
    let missing = execution.agent_outcome("codex", Path::new("."), None);
    assert!(missing.final_response.is_none());
}

#[test]
fn native_and_guest_apply_the_same_adapter_terminal_answer_policy() {
    let fixture = Fixture::new();
    for guest in [false, true] {
        let mut execution = evidence(0, guest);
        execution.output.stdout = b"{\"type\":\"tool_call\"}".to_vec();
        if let Some(metadata) = execution.guest.as_mut() {
            metadata.final_response = None;
        }
        let outcome = execution.agent_outcome("openjiuwen", &fixture.0, None);
        assert!(
            outcome
                .failure
                .unwrap()
                .contains("exited without a final response")
        );
        assert!(outcome.final_response.is_none());
        if let Some(metadata) = execution.guest.as_mut() {
            metadata.final_response = Some("finished".into());
        } else {
            execution.output.stdout = b"{\"type\":\"answer\",\"payload\":\"finished\"}".to_vec();
        }
        let outcome = execution.agent_outcome("openjiuwen", &fixture.0, None);
        assert!(outcome.failure.is_none());
        assert_eq!(outcome.final_response.as_deref(), Some("finished"));
    }
}

#[test]
fn durable_truncated_guest_transcript_keeps_its_sealed_terminal_metadata() {
    let mut execution = evidence(0, true);
    {
        let metadata = execution.guest.as_mut().expect("guest evidence");
        metadata.transcript_truncated = true;
        assert_eq!(metadata.final_response.as_deref(), Some("submission"));
    }
    assert!(ExecutionEvidence::capture_complete(
        &AttemptIssues::default(),
        execution.guest.as_ref()
    ));
    execution.capture_complete = true;
    let agent = execution.agent_outcome("pi", Path::new("."), None);
    assert!(agent.failure.is_none());
    assert_eq!(agent.final_response.as_deref(), Some("submission"));
    let grade = workloads::outcome::EvaluationOutcome::binary(true);
    let decision = crate::runner::disposition::RunDisposition::from_facts(
        crate::runner::disposition::RunFacts {
            evaluation: &grade,
            evaluation_kind: workloads::KnownEvaluationKind::UpstreamHarness,
            process_success: agent.failure.is_none(),
            agent_failure: agent.failure.as_deref(),
            capture_incomplete: true,
            run_error: None,
            collector_error: None,
            output_error: None,
            provider_cleanup_error: None,
            workload_cleanup_confirmed: true,
            proxy_cleanup_confirmed: true,
        },
    );
    assert_eq!(
        decision.status,
        crate::runner::disposition::RunStatus::Completed
    );
    assert!(decision.success);
    execution
        .guest
        .as_mut()
        .expect("guest evidence")
        .transcript_truncated = false;
    assert!(ExecutionEvidence::capture_complete(
        &AttemptIssues::default(),
        execution.guest.as_ref()
    ));
}

#[test]
fn native_reward_pending_and_error_survive_marker_and_cleanup_failures() {
    use workloads::outcome::EvaluationOutcome as Evaluation;
    for grade in [3, 4, 5] {
        for event in [
            LifecycleEvent::PhaseBegin(LifecyclePhase::WorkloadEvaluation),
            LifecycleEvent::PhaseEnd(LifecyclePhase::WorkloadEvaluation),
            LifecycleEvent::PhaseBegin(LifecyclePhase::WorkloadCleanup),
        ] {
            let fixture = Fixture::new();
            let mut session =
                measurements::Session::start_with_collectors(&fixture.0.join("measure"), &[], None)
                    .unwrap();
            session.test_event_failure = Some(event);
            let (prepared, operations) = prepared(&fixture, grade, true, Some(session));
            let result = prepared
                .executed(evidence(7, false), AttemptIssues::default())
                .settle(None)
                .evaluate("submission")
                .finish();
            match &result.outcome.evaluation {
                Evaluation::Completed {
                    resolution,
                    rewards,
                    ..
                } => {
                    assert_eq!(grade, 3);
                    assert_eq!(*resolution, None);
                    assert_eq!(rewards["accuracy"], 0.5);
                }
                Evaluation::Pending { .. } => assert_eq!(grade, 4),
                Evaluation::Error { .. } => assert_eq!(grade, 5),
            }
            assert_eq!(&*operations.borrow(), &["evaluate", "finish"]);
            assert_eq!(result.outcome.success(), None);
            assert_eq!(result.evaluator_evidence.len(), 1);
            assert!(!result.cleanup_confirmed);
            assert!(result.issues.collector_failure().is_some());
            assert!(result.issues.run_error.is_some());
        }
    }
}

#[test]
fn every_evaluation_and_cleanup_marker_failure_still_invokes_owned_operations() {
    for event in [
        LifecycleEvent::PhaseBegin(LifecyclePhase::WorkloadEvaluation),
        LifecycleEvent::PhaseEnd(LifecyclePhase::WorkloadEvaluation),
        LifecycleEvent::PhaseFailed(LifecyclePhase::WorkloadEvaluation),
        LifecycleEvent::PhaseBegin(LifecyclePhase::WorkloadCleanup),
        LifecycleEvent::PhaseEnd(LifecyclePhase::WorkloadCleanup),
        LifecycleEvent::PhaseFailed(LifecyclePhase::WorkloadCleanup),
    ] {
        for grade in [0, 1, 2] {
            let fixture = Fixture::new();
            let mut session =
                measurements::Session::start_with_collectors(&fixture.0.join("measure"), &[], None)
                    .unwrap();
            session.test_event_failure = Some(event);
            let (prepared, operations) = prepared(&fixture, grade, true, Some(session));
            let result = prepared
                .executed(evidence(0, false), AttemptIssues::default())
                .settle(None)
                .evaluate("submission")
                .finish();
            assert_eq!(
                result.outcome.success(),
                match grade {
                    0 => Some(true),
                    1 => Some(false),
                    _ => None,
                }
            );
            assert_eq!(operations.borrow().len(), 2);
            if event != LifecycleEvent::PhaseFailed(LifecyclePhase::WorkloadEvaluation)
                || grade != 0
            {
                assert!(
                    result
                        .issues
                        .collector_failure()
                        .unwrap()
                        .contains("fixture collector event failure")
                );
            }
            assert!(
                result
                    .issues
                    .run_error
                    .unwrap()
                    .contains("owned cleanup unavailable")
            );
        }
    }
}

#[test]
fn interrupted_prepared_or_executed_attempt_preserves_without_grading() {
    for executed in [false, true] {
        let fixture = Fixture::new();
        let (prepared, operations) = prepared(&fixture, 0, false, None);
        if executed {
            drop(prepared.executed(evidence(0, false), AttemptIssues::default()));
        } else {
            drop(prepared);
        }
        assert_eq!(operations.borrow().as_slice(), ["cancel-preserve"]);
    }
}

#[test]
fn empty_missing_records_and_replay_failure_never_skip_grading_or_invent_usage() {
    for mode in [tape::Mode::Record, tape::Mode::Replay] {
        for empty in [false, true] {
            let fixture = Fixture::new();
            let path = fixture.0.join("cassette");
            if empty {
                std::fs::write(&path, b"").unwrap();
            }
            let (prepared, operations) = prepared(&fixture, 0, false, None);
            let mut issues = AttemptIssues::default();
            if mode == tape::Mode::Replay {
                issues.run_error("partial replay mismatch".into());
            }
            let result = prepared
                .executed(evidence(0, false), issues)
                .settle(Some(&(mode, path)))
                .evaluate("submission")
                .finish();
            assert_eq!(result.outcome.success(), Some(true));
            assert_eq!(operations.borrow().as_slice(), ["evaluate", "finish"]);
            assert!(result.issues.run_error.is_some());
            assert_eq!(result.cassette_usage_verified, mode == tape::Mode::Record);
        }
    }
}

#[test]
fn delayed_postexecution_work_does_not_extend_frozen_execution_time() {
    let fixture = Fixture::new();
    let (prepared, _) = prepared(&fixture, 0, false, None);
    let executed = prepared.executed(evidence(0, false), AttemptIssues::default());
    std::thread::sleep(Duration::from_millis(30));
    let result = executed.settle(None).evaluate("submission").finish();
    assert_eq!(result.evidence.elapsed_ms, 17);
}

#[test]
fn actual_native_duration_excludes_slow_execution_end_collector_hook() {
    struct SlowEnd;
    impl crate::collectors::CollectorAdapter for SlowEnd {
        fn id(&self) -> &'static str {
            "fixture-slow-end"
        }
        fn lifecycle_event(
            &self,
            _: &mut measurements::Session,
            event: LifecycleEvent,
        ) -> Result<(), String> {
            if event == LifecycleEvent::AgentExecuteEnd {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        }
    }
    static SLOW: SlowEnd = SlowEnd;
    let fixture = Fixture::new();
    let mut session =
        measurements::Session::start_with_collectors(&fixture.0.join("measure"), &[], None)
            .unwrap();
    session.add_test_adapter(&SLOW);
    let mut command = Command::new("/bin/true");
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    let started = Instant::now();
    let target = crate::launch::TargetLaunch::direct(&command);
    let output = run_measured_agent(
        &mut command,
        &target,
        None,
        Some(&mut session),
        false,
        &mut Vec::new(),
    )
    .unwrap();
    assert!(output.output.status.success());
    assert!(
        started.elapsed().as_millis() >= output.elapsed_ms + 90,
        "collector shutdown must not enter execution elapsed_ms"
    );
}

#[test]
fn broken_native_terminal_retains_capture_and_finishes_already_admitted_work() {
    struct Broken {
        writes: usize,
        flush_only: bool,
    }
    impl Write for Broken {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            if self.flush_only {
                Ok(bytes.len())
            } else {
                Err(std::io::Error::other("first presentation failure"))
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("first flush failure"))
        }
    }
    for flush_only in [false, true] {
        let fixture = Fixture::new();
        let mut writer = Broken {
            writes: 0,
            flush_only,
        };
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf submission; printf diagnostic >&2; exit 7"])
            .env_clear()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let target = crate::launch::TargetLaunch::direct(&command);
        let measured =
            run_measured_agent(&mut command, &target, None, None, true, &mut writer).unwrap();
        assert_eq!(
            writer.writes, 1,
            "never retry a broken terminal for every chunk/stream"
        );
        assert_eq!(measured.output.stdout, b"submission");
        assert_eq!(measured.output.stderr, b"diagnostic");
        assert_eq!(measured.output.status.code(), Some(7));
        assert!(
            measured
                .issues
                .output_error
                .as_ref()
                .unwrap()
                .contains(if flush_only {
                    "first flush failure"
                } else {
                    "first presentation failure"
                })
        );
        let (prepared, operations) = prepared(&fixture, 0, false, None);
        let mut execution = evidence(7, false);
        execution.output = measured.output;
        let finished = prepared
            .executed(execution, measured.issues)
            .settle(None)
            .evaluate("submission")
            .finish();
        assert_eq!(finished.outcome.success(), Some(true));
        assert_eq!(operations.borrow().as_slice(), ["evaluate", "finish"]);
        assert!(finished.issues.output_error.is_some());
    }
}

#[test]
fn native_postexecution_event_failure_retains_actual_child_output_and_grade() {
    for event in [LifecycleEvent::AgentExecuteEnd, LifecycleEvent::AgentExited] {
        let fixture = Fixture::new();
        let mut session =
            measurements::Session::start_with_collectors(&fixture.0.join("measure"), &[], None)
                .unwrap();
        session.test_event_failure = Some(event);
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf submission; printf diagnostic >&2; exit 7"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        let target = crate::launch::TargetLaunch::direct(&command);
        let measured = run_measured_agent(
            &mut command,
            &target,
            None,
            Some(&mut session),
            false,
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(measured.output.stdout, b"submission");
        assert_eq!(measured.output.status.code(), Some(7));
        assert!(
            measured
                .issues
                .collector_failure()
                .unwrap()
                .contains("fixture collector event failure")
        );
        let (prepared, operations) = prepared(&fixture, 0, false, Some(session));
        let mut evidence = evidence(7, false);
        evidence.output = measured.output;
        evidence.elapsed_ms = measured.elapsed_ms;
        let result = prepared
            .executed(evidence, measured.issues)
            .settle(None)
            .evaluate("submission")
            .finish();
        assert_eq!(result.outcome.success(), Some(true));
        assert_eq!(operations.borrow().as_slice(), ["evaluate", "finish"]);
    }
}

#[path = "runner_bundle_publication.rs"]
mod bundle_publication;
#[path = "runner_publication.rs"]
mod publication;

fn recording_identity(result: &RunResult<'_>) -> crate::cassette::Compatibility {
    crate::cassette::Compatibility {
        agent: result.agent.into(),
        provider: result.provider.into(),
        model: result.model.into(),
        workload: result.workload.into(),
        task: result.task.task_id.clone(),
        dataset_sha256: None,
        prompt_sha256: "a".repeat(64),
        material_sha256: "b".repeat(64),
        protocol_sha256: Some("c".repeat(64)),
    }
}

#[test]
fn recording_association_binds_actual_attempt_and_preserves_uncertain_raw_capture() {
    let fixture = Fixture::new();
    let campaign = fixture.0.canonicalize().unwrap().join("campaign-1");
    std::fs::create_dir_all(campaign.join("batch-1")).unwrap();
    let destination =
        super::super::recording::preflight(&fixture.0.join("recording.asb.zip")).unwrap();
    let recorder = super::super::recording::Recorder::new(
        destination,
        campaign.clone(),
        "campaign-1".into(),
        1,
        1,
        None,
    );
    let result = result();
    let identity = recording_identity(&result);
    recorder.prepare(&identity, 1, &[1]).unwrap();
    let producer = super::super::recording::base(&campaign, 1);
    std::fs::write(&producer, b"{}\n").unwrap();
    let capture = crate::proxy::capture::CaptureEvidence {
        adapter: "llmtape".into(),
        output: producer.clone(),
        complete: true,
        accepted: 1,
        completed: 1,
        failed: 0,
        recorded: 1,
        inference_requests: 1,
        bytes: 3,
        sha256: crate::cassette::digest(b"{}\n"),
    };
    let proxy = crate::proxy::Evidence {
        captures: vec![capture.clone()],
        output_files: vec![producer.clone()],
        cleanup_confirmed: false,
    };
    let evidence = recorder
        .commit(1, replica::Identity::default(), &result, &proxy)
        .unwrap();
    let mut bytes = Vec::new();
    render_json(&result, &mut bytes).unwrap();
    let mut summary: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    summary["execution"] =
        serde_json::json!({"campaign":"campaign-1","batch":1,"replica":1,"slot":1});
    evidence
        .validate_summary(&summary, &proxy.captures)
        .unwrap();
    for (field, value) in [
        ("batch", serde_json::json!(2)),
        ("replica", serde_json::json!(2)),
        ("slot", serde_json::json!(2)),
        ("campaign", serde_json::json!("foreign")),
    ] {
        let mut altered = summary.clone();
        altered["execution"][field] = value;
        assert!(
            evidence
                .validate_summary(&altered, &proxy.captures)
                .is_err(),
            "{field}"
        );
    }
    let mut forged = evidence.clone();
    forged.attempt_id = "another-attempt".into();
    let mut altered = summary.clone();
    altered["task"]["attempt_id"] = serde_json::json!("another-attempt");
    assert!(
        forged
            .validate_summary(&altered, &proxy.captures)
            .unwrap_err()
            .to_string()
            .contains("association changed")
    );
    assert!(
        evidence
            .validate_summary(&summary, &[capture.clone(), capture])
            .is_err()
    );
    assert!(evidence.validate_summary(&summary, &[]).is_err());
    let published = recorder.finalize(false).unwrap().unwrap();
    assert!(!published.cleanup_complete);
    assert!(producer.exists());
    evidence
        .validate_summary(&summary, &proxy.captures)
        .unwrap();
}

#[test]
fn recording_setup_failure_retains_partial_bytes_with_unknown_outcomes() {
    let fixture = Fixture::new();
    let campaign = fixture.0.canonicalize().unwrap().join("campaign-1");
    std::fs::create_dir_all(campaign.join("batch-1")).unwrap();
    let destination =
        super::super::recording::preflight(&fixture.0.join("partial.asb.zip")).unwrap();
    let recorder = super::super::recording::Recorder::new(
        destination.clone(),
        campaign.clone(),
        "campaign-1".into(),
        1,
        1,
        None,
    );
    recorder
        .prepare(&recording_identity(&result()), 1, &[1])
        .unwrap();
    let producer = super::super::recording::base(&campaign, 1);
    std::fs::write(&producer, b"partial trace").unwrap();
    recorder.finalize(false).unwrap().unwrap();
    let mut archive =
        crate::cassette::Archive::open(&destination, crate::cassette::Limits::default()).unwrap();
    let track = &archive.manifest().tracks[0];
    assert!(!track.complete);
    assert_eq!(track.process_success, None);
    assert_eq!(track.workload_success, None);
    assert_eq!(track.statistics, crate::cassette::Statistics::default());
    assert_eq!(archive.read_track(1).unwrap(), b"partial trace");
    assert!(producer.exists());
}

fn result() -> RunResult<'static> {
    RunResult {
        observation: Default::default(),
        recording: None,
        execution: None,
        workload_resource_disposition: Some(workloads::ResourceDisposition::Removed),
        workload_artifacts: Vec::new(),
        planning: crate::planning::resolve(
            &toml::from_str("agent='pi'\nprovider='fixture'\nmodel='fixture'\nworkload='fixture'")
                .unwrap(),
        )
        .unwrap(),
        effective_budgets: Default::default(),
        execution_plan: None,
        execution_plan_fingerprint: None,
        module: None,
        benchmark_outcome: None,
        replica_index: Some(1),
        execution_cohort: None,
        schema_version: 2,
        disposition: RunDisposition {
            status: RunStatus::Completed,
            failure: None,
            success: true,
        },
        agent: "fixture",
        workload: "fixture",
        task: workloads::TaskIdentity::local("fixture", None),
        provider: "fixture",
        model: "fixture",
        process_success: true,
        workload_success: Some(true),
        workload_evaluation: "upstream-harness",
        workload_baseline_passed: Some(false),
        workload_baseline: workloads::BaselineEvidence::Measured {
            passed: Some(false),
        },
        workload_failure: None,
        success_rate: Some(1.0),
        adapter_smoke_success: None,
        elapsed_ms: 17,
        exit_code: Some(0),
        token_usage: Default::default(),
        steps: Some(3),
        tool_calls: Vec::new(),
        tool_calls_available: true,
        native_trace: None,
        native_tool_sidecar: None,
        local_tool_transcript: None,
        deterministic_manifest: None,
        sealed_artifacts: Vec::new(),
        cassette: None,
        measurement_directory: None,
        stdout: "submission".into(),
        stderr: String::new(),
    }
}

#[test]
fn postgrade_log_publication_is_independent_and_diagnostics_keep_primary_order() {
    let fixture = Fixture::new();
    let config = fixture.0.join("benchmark.toml");
    let result = result();
    let before = serde_json::to_vec(&result).unwrap();
    let published = LogPublication::publish(&config, "nested/run.jsonl", &result);
    assert!(published.confirmed());
    assert!(final_outcome(None, published, MeasurementCompletion::Confirmed).is_ok());
    let logged: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("nested/run.jsonl")).unwrap())
            .unwrap();
    assert_eq!(logged["workload_success"], true);
    assert_eq!(logged["workload_resource_disposition"], "removed");
    assert_eq!(logged["workload_artifacts"], serde_json::json!([]));
    std::fs::create_dir(fixture.0.join("blocked.jsonl")).unwrap();
    for primary in [
        None,
        Some(RunFailure {
            kind: FailureKind::RunError,
            reason: "original proxy failure".into(),
        }),
    ] {
        let failed = LogPublication::publish(&config, "blocked.jsonl", &result);
        assert!(!failed.confirmed());
        let error = final_outcome(
            primary.clone(),
            failed,
            MeasurementCompletion::Incomplete(vec!["measurement write failure".into()]),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("regular file"));
        assert!(error.contains(fixture.0.join("blocked.jsonl").to_str().unwrap()));
        assert!(error.contains("measurement write failure"));
        if primary.is_some() {
            assert!(
                error.find("original proxy failure").unwrap()
                    < error.find("run log publication failed").unwrap()
            );
        }
        assert!(
            error.find("run log publication failed").unwrap()
                < error.find("measurement finalization failed").unwrap()
        );
    }
    let failed = LogPublication::publish(&config, "blocked.jsonl", &result);
    assert!(
        final_outcome(None, failed, MeasurementCompletion::Confirmed)
            .unwrap_err()
            .to_string()
            .contains("run log publication failed after task evaluation")
    );
    assert_eq!(serde_json::to_vec(&result).unwrap(), before);
}

#[test]
fn final_measurement_errors_preserve_grade_and_disallow_completion_receipts() {
    for fault in [
        "run-failed",
        "run-finishing",
        "publication",
        "none",
        "disabled",
    ] {
        let fixture = Fixture::new();
        let dir = fixture.0.join("measurements");
        let mut session = measurements::Session::start_with_collectors(&dir, &[], None).unwrap();
        session.test_event_failure = match fault {
            "run-failed" => Some(LifecycleEvent::RunFailed),
            "run-finishing" => Some(LifecycleEvent::RunFinishing),
            _ => None,
        };
        if fault == "publication" {
            std::fs::create_dir(dir.join("run.json")).unwrap();
        }
        let mut result = result();
        if fault == "run-failed" {
            result.disposition = RunDisposition {
                status: RunStatus::Failed,
                success: false,
                failure: Some(RunFailure {
                    kind: FailureKind::RunError,
                    reason: "fixture operational fault".into(),
                }),
            };
        }
        let before = serde_json::to_vec(&result).unwrap();
        let selected = if fault == "disabled" {
            drop(session);
            None
        } else {
            Some(session)
        };
        let completed = finish_measurements(selected, &result);
        assert_eq!(completed.confirmed(), matches!(fault, "none" | "disabled"));
        let error = completed.into_error();
        if !matches!(fault, "none" | "disabled") {
            let error = error.unwrap().to_string();
            assert!(error.contains("after task evaluation"));
            assert!(error.contains("do not repeat the agent"));
        }
        assert_eq!(
            serde_json::to_vec(&result).unwrap(),
            before,
            "a finalization fault cannot replace the grade"
        );
        let mut output = Vec::new();
        render_json(&result, &mut output).unwrap();
        let rendered: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(rendered["workload_success"], true);
        assert_eq!(rendered["success_rate"], 1.0);
        if matches!(fault, "run-failed" | "run-finishing" | "none") {
            let measured: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.join("run.json")).unwrap()).unwrap();
            assert_eq!(
                measured["workload_success"], true,
                "finalization must still attempt publication after marker failure"
            );
            assert_eq!(measured["workload_resource_disposition"], "removed");
            assert_eq!(measured["workload_artifacts"], serde_json::json!([]));
        }
    }
}

#[test]
fn unverified_replay_does_not_publish_a_zero_tool_call_metric() {
    let fixture = Fixture::new();
    let dir = fixture.0.join("measurements");
    let session = measurements::Session::start_with_collectors(&dir, &[], None).unwrap();
    let mut result = result();
    result.tool_calls_available = false;
    assert!(finish_measurements(Some(session), &result).confirmed());
    let run: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("run.json")).unwrap()).unwrap();
    assert!(run.get("tool_call_count").is_none());
    let calls: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("tool-calls.json")).unwrap()).unwrap();
    assert_eq!(calls["availability"], "unavailable");
    assert!(calls.get("count").is_none());
}

#[test]
fn full_runner_entry_keeps_real_fixture_grade_and_primary_error_after_lifecycle_failure() {
    const CASE: &str = "ASB_RUNNER_ENTRY_TEST_CASE";
    if let Ok(case) = std::env::var(CASE) {
        full_entry_child(&case);
        return;
    }
    isolated_full_entry_cases(&[
        "execution-end",
        "run-finishing",
        "blocked-log",
        "blocked-log-and-measurements",
    ]);
}

#[test]
fn full_runner_entry_keeps_grade_and_publication_errors_when_result_writer_also_breaks() {
    isolated_full_entry_cases(&["blocked-log-broken-output"]);
}

#[test]
fn runner_phase_completion_red_stopped_warning_true_publishes_grade() {
    isolated_full_entry_cases(&["phase-warning-true"]);
    #[cfg(target_os = "linux")]
    isolated_full_entry_cases(&["phase-warning-true-observation-fault"]);
}

#[test]
fn runner_phase_completion_red_stopped_warning_false_publishes_grade() {
    isolated_full_entry_cases(&["phase-warning-false"]);
    #[cfg(target_os = "linux")]
    isolated_full_entry_cases(&["phase-warning-false-observation-fault"]);
}

#[test]
fn runner_phase_completion_red_stopped_warning_native_publishes_grade() {
    isolated_full_entry_cases(&["phase-warning-native"]);
    #[cfg(target_os = "linux")]
    isolated_full_entry_cases(&["phase-warning-native-observation-fault"]);
}

#[test]
fn runner_phase_completion_red_removed_cwd_preserves_anchored_measurements() {
    isolated_full_entry_cases(&["phase-removed-cwd"]);
}

#[test]
fn runner_phase_completion_composes_all_independent_postgrade_failures() {
    isolated_full_entry_cases(&[
        "phase-warning-true-provider-blocked-log-measurements",
        "phase-warning-false-provider-blocked-log-measurements",
        "phase-warning-native-provider-blocked-log-measurements",
    ]);
}

#[test]
fn runner_phase_completion_owns_prepared_workload_when_end_instrumentation_fails() {
    isolated_full_entry_cases(&[
        "phase-prepare-workload-end",
        "phase-prepare-workload-end-provider",
    ]);
}

#[test]
fn runner_phase_completion_warning_io_stops_repeat_but_confirmed_grades_remain_reusable() {
    isolated_full_entry_cases(&[
        "phase-warning-repeat-true",
        "phase-warning-repeat-false",
        "phase-warning-repeat-native",
        "phase-warning-sweep-true",
        "phase-warning-sweep-false",
        "phase-warning-sweep-native",
    ]);
}

#[test]
fn runner_phase_completion_retains_workload_owner_through_publication_and_observer() {
    isolated_full_entry_cases(&["phase-owner-lifetime"]);
}

#[test]
fn runner_phase_completion_dispatch_failure_never_uses_prepared_only_cancellation() {
    isolated_full_entry_cases(&["phase-runtime-error", "phase-runtime-error-provider"]);
}

#[test]
fn full_runner_entry_observer_warning_is_redacted_and_broken_warning_writer_keeps_primary() {
    isolated_full_entry_cases(&["observer-failure", "observer-warning-writer"]);
}

#[test]
fn full_runner_native_artifact_handoff_survives_publication_loss_without_repeating_owners() {
    isolated_full_entry_cases(&["artifact-loss-disabled", "artifact-loss-failed-measurement"]);
}

#[test]
fn full_runner_entry_explicit_provider_finish_preserves_grades_and_primary_failures() {
    isolated_full_entry_cases(&[
        "provider-true-failed-finish",
        "provider-false-failed-finish",
        "provider-client-failed-finish",
        "provider-early-failed-finish",
        "provider-success",
        "provider-native-failed-finish",
        "provider-native-success",
    ]);
}

#[test]
fn full_runner_entry_disposition_keeps_pending_and_operational_failures_consistent() {
    isolated_full_entry_cases(&[
        "disposition-pending",
        "disposition-pending-collector",
        "disposition-pending-output",
        "disposition-pending-collector-output",
        "disposition-pending-provider",
        "disposition-pending-client",
        "disposition-pending-run",
        "disposition-pending-cleanup",
        "disposition-grader",
        "disposition-true-output",
        "disposition-false-output",
        "disposition-native-output",
    ]);
}

#[test]
fn full_runner_entry_bounded_capture_preserves_independent_file_grades_without_receipts() {
    isolated_full_entry_cases(&[
        "capture-true-limit",
        "capture-false-limit",
        "capture-native-limit",
        "capture-true-read-failure",
        "capture-mini-swe-read-failure",
    ]);
}

#[test]
fn missing_fixture_grade_reports_settled_agent_and_proxy_evidence() {
    let fixture = Fixture::new();
    let root = fixture.0.canonicalize().unwrap();
    let _scope = entry_test_support::Scope::disposition(
        &root,
        Rc::new(RefCell::new(Vec::new())),
        "termination-true-code143",
    );
    let mut execution = evidence(9, false);
    execution.output.stdout = b"private-sentinel-agent-stdout agent_end".to_vec();
    execution.output.stderr =
        b"fixture replay request failed native trace observed an unowned child status private-sentinel-agent-stderr".to_vec();
    let issues = AttemptIssues {
        run_error: Some(format!(
            "proxy completion failed private-sentinel-error {}",
            "x".repeat(1_000_000)
        )),
        capture_error: Some(format!(
            "private-sentinel-capture {}",
            "y".repeat(1_000_000)
        )),
        ..Default::default()
    };
    let proxy = crate::proxy::Evidence {
        captures: Vec::new(),
        output_files: Vec::new(),
        cleanup_confirmed: false,
    };
    entry_test_support::capture_grade_diagnostic(&root, &execution, &issues, &proxy);
    let result = std::panic::catch_unwind(|| {
        entry_test_support::read_fixture_grade(&root.join("private-sentinel-grade"))
    });
    let error = result.expect_err("missing grade must fail the fixture");
    let message = error
        .downcast_ref::<String>()
        .expect("diagnostic panic string");
    assert!(message.contains("fixture-replay-failed"), "{message}");
    assert!(message.contains("gate-unowned-child-status"), "{message}");
    assert!(message.contains("proxy-completion-failed"), "{message}");
    assert!(
        message.contains("proxy_cleanup_confirmed=false"),
        "{message}"
    );
    assert!(message.contains("process_status_code="), "{message}");
    assert!(message.contains("run_error_bytes="), "{message}");
    assert!(message.contains("capture_error_bytes="), "{message}");
    assert!(!message.contains("private-sentinel"), "{message}");
    assert!(!message.contains(root.to_str().unwrap()), "{message}");
    assert!(message.len() < 2048, "diagnostic must stay bounded");
}

#[test]
#[cfg(target_os = "linux")]
fn actual_native_termination_keeps_grades_and_optional_faults_do_not_change_completion() {
    isolated_full_entry_cases(&[
        "termination-true-code137",
        "termination-false-code137",
        "termination-pending-code137",
        "termination-true-code143",
        "termination-false-code143",
        "termination-pending-code143",
        "termination-true-signal9",
        "termination-false-signal9",
        "termination-pending-signal9",
        "termination-true-signal15",
        "termination-false-signal15",
        "termination-pending-signal15",
        "termination-true-code0",
        "termination-false-code0",
        "termination-pending-code0",
        "termination-true-observation-fault",
        "termination-false-observation-fault",
        "termination-pending-observation-fault",
        "termination-repeat-true-code0",
        "termination-repeat-false-code0",
        "termination-repeat-true-observation-fault",
        "termination-repeat-false-observation-fault",
    ]);
}

#[test]
#[cfg(target_os = "linux")]
fn actual_guest_termination_reaches_true_false_pending_grade_phases_without_reexecution() {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use support::IsolatedEnvironment;
    let runtime = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    for (ending, exact, carrier) in [
        (
            "exit 137",
            serde_json::json!({"kind":"code","code":137}),
            137,
        ),
        (
            "exit 143",
            serde_json::json!({"kind":"code","code":143}),
            143,
        ),
        (
            "kill -KILL $$",
            serde_json::json!({"kind":"signal","signal":9}),
            1,
        ),
        (
            "kill -TERM $$",
            serde_json::json!({"kind":"signal","signal":15}),
            1,
        ),
    ] {
        for grade in [0, 1, 4] {
            let fixture = Fixture::new();
            let root = fixture.0.canonicalize().unwrap();
            for name in [".asb", "tools/pi/bin", "workspace"] {
                std::fs::create_dir_all(root.join(name)).unwrap();
            }
            std::fs::write(
                root.join(".asb/project.toml"),
                "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
            )
            .unwrap();
            let program = root.join("tools/pi/bin/pi");
            crate::write_test_file(&root, &program,format!("#!/bin/sh\nprintf 'called\\n' >> '{}'\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"submission\"}}'\n{ending}\n",root.join("client-calls").display())).unwrap();
            std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o700)).unwrap();
            let request = serde_json::json!({"config":{"agent":"pi","provider":"openrouter","model":"fixture","workload":"fixture","workspace":root.join("workspace")},"connection":{"interface":"open_ai_chat","authentication":"bearer","base_url":"http://127.0.0.1:1/v1","credential":"fixture"},"prompt":"offline","project_root":root,"runtime_dir":root.join("private"),"run_dir":root.join("private/attempt")});
            let mut command = Command::new(&runtime);
            command
                .arg("__asb-agent")
                .isolated_env()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", &root)
                .env("ASB_GUEST_AGENT_REQUEST", request.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = OwnedChild::spawn_direct(&mut command).unwrap();
            let stdout = child.take_stdout().unwrap();
            child.resume().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            };
            assert_eq!(status.code(), Some(carrier));
            let mut bytes = Vec::new();
            stdout.take(4096).read_to_end(&mut bytes).unwrap();
            let metadata: agents::driver::GuestResult = serde_json::from_slice(
                &std::fs::read(root.join("private/attempt/agent-result.json")).unwrap(),
            )
            .unwrap();
            assert!(metadata.tree_quiesced);
            assert_eq!(
                serde_json::to_value(metadata.observation.process_termination).unwrap(),
                exact
            );
            let (prepared, operations) = prepared(&fixture, grade, false, None);
            let final_state = prepared
                .executed(
                    ExecutionEvidence {
                        observation: metadata.observation,
                        artifact_owner: None,
                        output: Output {
                            status,
                            stdout: bytes,
                            stderr: Vec::new(),
                        },
                        capture_complete: true,
                        run_dir: PathBuf::new(),
                        guest: Some(metadata),
                        elapsed_ms: 1,
                        runtime_output: None,
                    },
                    AttemptIssues::default(),
                )
                .settle(None)
                .evaluate("submission")
                .finish();
            assert_eq!(
                final_state.outcome.success(),
                match grade {
                    0 => Some(true),
                    1 => Some(false),
                    _ => None,
                }
            );
            assert!(final_state.cleanup_confirmed);
            assert!(final_state.issues.run_error.is_none());
            assert_eq!(final_state.evidence.output.status.code(), Some(carrier));
            assert_eq!(
                std::fs::read_to_string(root.join("client-calls")).unwrap(),
                "called\n"
            );
            assert_eq!(operations.borrow().as_slice(), ["evaluate", "finish"]);
        }
    }
}

#[test]
fn actual_unmeasured_sweep_capture_failure_keeps_grades_reporting_only() {
    isolated_full_entry_cases(&[
        "capture-sweep-true-limit",
        "capture-sweep-false-limit",
        "capture-sweep-native-limit",
    ]);
}

#[test]
fn repeated_provider_finish_uncertainty_stops_batches_without_losing_grades() {
    isolated_full_entry_cases(&[
        "provider-repeat-failed-finish",
        "provider-repeat-grader-failed-finish",
        "provider-repeat-grader-success",
    ]);
}

fn isolated_full_entry_cases(cases: &[&str]) {
    use support::IsolatedEnvironment;
    let executable = std::env::current_exe().unwrap();
    let runtime = executable.parent().unwrap().parent().unwrap().join("asb");
    assert!(
        runtime.is_file(),
        "build the actual ASB binary before this full-entry test"
    );
    for case in cases {
        let child = Command::new(&executable)
            .args(["--exact", "runner::phases::tests::full_runner_entry_keeps_real_fixture_grade_and_primary_error_after_lifecycle_failure", "--nocapture"])
            .isolated_env()
            .env("ASB_RUNNER_ENTRY_TEST_CASE", case)
            // The child is a separately executed test binary, so it cannot
            // inherit the parent test thread's capacity fixture. Keep this
            // premise explicit rather than letting host free space decide an
            // unrelated lifecycle qualification.
            .env("ASB_TEST_AVAILABLE_MIB", "100000")
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("ASB_RUNTIME_EXECUTABLE", &runtime)
            .output().unwrap();
        assert!(
            child.status.success(),
            "{case}: {}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
    }
}

#[test]
fn early_provider_teardown_notifies_material_peers_before_waiting_for_cleanup() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let gate = super::super::batch::Gate::new(2, None, Duration::from_secs(10)).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let (sent, received) = std::sync::mpsc::channel();
    struct Provider {
        received: std::sync::mpsc::Receiver<()>,
        calls: Arc<AtomicUsize>,
    }
    impl providers::ProviderSession for Provider {
        fn api_key(&self) -> &str {
            "fixture"
        }
        fn base_url(&self) -> &str {
            "http://127.0.0.1:1"
        }
        fn finish(&mut self) -> Result<(), Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.received
                .recv_timeout(Duration::from_secs(2))
                .expect("peer must observe abort before provider teardown waits");
            Err(Error::Run("fixture slow provider cleanup failure".into()))
        }
    }
    std::thread::scope(|scope| {
        let mut arrival = Some(
            gate.register(replica::Identity { index: 1, total: 2 })
                .unwrap(),
        );
        let mut peer = gate
            .register(replica::Identity { index: 2, total: 2 })
            .unwrap();
        let handle = scope.spawn(move || {
            let identity = crate::cassette::Compatibility {
                agent: "fixture".into(),
                provider: "fixture".into(),
                model: "fixture".into(),
                workload: "fixture".into(),
                task: "fixture".into(),
                dataset_sha256: None,
                prompt_sha256: "a".repeat(64),
                material_sha256: "b".repeat(64),
                protocol_sha256: None,
            };
            assert!(
                peer.ready(identity)
                    .unwrap_err()
                    .to_string()
                    .contains("early fixture")
            );
            sent.send(()).unwrap();
        });
        let mut provider: Option<Box<dyn providers::ProviderSession>> = Some(Box::new(Provider {
            received,
            calls: calls.clone(),
        }));
        let result = super::super::finish_provider_attempt(
            Err(Error::Io("early fixture output failure".into())),
            &mut arrival,
            &mut provider,
        );
        assert!(
            matches!(&result, Err(Error::Io(reason)) if reason.contains("early fixture") && reason.contains("provider cleanup failed"))
        );
        assert!(provider.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        handle.join().unwrap();
    });
}

#[test]
fn runner_phase_completion_preparation_abort_precedes_owned_workload_cleanup() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct PreparedWorkload {
        invocation: workloads::session::TaskInvocation,
        peer_aborted: std::sync::mpsc::Receiver<()>,
        cancellations: Arc<AtomicUsize>,
    }
    impl workloads::WorkloadSession for PreparedWorkload {
        fn invocation(&self) -> &workloads::session::TaskInvocation {
            &self.invocation
        }
        fn evaluation_kind(&self) -> workloads::KnownEvaluationKind {
            workloads::KnownEvaluationKind::BundledFixture
        }
        fn baseline_passed(&self) -> Option<bool> {
            Some(false)
        }
        fn evaluate(&mut self, _: &str) -> Result<workloads::Evaluation, String> {
            panic!("aborted preparation cannot grade");
        }
        fn cancel_prepared(&mut self) -> Result<(), String> {
            self.cancellations.fetch_add(1, Ordering::SeqCst);
            self.peer_aborted
                .recv_timeout(Duration::from_secs(2))
                .expect("peer must observe abort before workload cancellation waits");
            Err("fixture uncertain preparation cleanup".into())
        }
        fn finish(&mut self) -> Result<workloads::session::ResourceSettlement, String> {
            panic!("aborted preparation cannot authorize normal destructive close");
        }
    }
    for kind in ["input", "io", "run"] {
        let gate = batch::Gate::new(2, None, Duration::from_secs(10)).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let count = Arc::new(AtomicUsize::new(0));
        let mut workload = PreparedWorkload {
            invocation: workloads::session::TaskInvocation {
                identity: workloads::TaskIdentity::local("fixture", None),
                input: "private fixture".into(),
                workspace: PathBuf::new(),
            },
            peer_aborted: received,
            cancellations: count.clone(),
        };
        std::thread::scope(|scope| {
            let mut arrival = Some(
                gate.register(replica::Identity { index: 1, total: 2 })
                    .unwrap(),
            );
            let mut peer = gate
                .register(replica::Identity { index: 2, total: 2 })
                .unwrap();
            let peer = scope.spawn(move || {
                let error = peer
                    .ready(crate::cassette::Compatibility {
                        agent: "fixture".into(),
                        provider: "fixture".into(),
                        model: "fixture".into(),
                        workload: "fixture".into(),
                        task: "fixture".into(),
                        dataset_sha256: None,
                        prompt_sha256: "a".repeat(64),
                        material_sha256: "b".repeat(64),
                        protocol_sha256: None,
                    })
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("primary preparation failure"));
                sent.send(()).unwrap();
            });
            let primary = match kind {
                "input" => Error::Input("primary preparation failure".into()),
                "io" => Error::Io("primary preparation failure".into()),
                _ => Error::Run("primary preparation failure".into()),
            };
            let error =
                super::super::attempt::cancel_prepared(primary, &mut arrival, &mut workload);
            assert!(matches!(
                (&error, kind),
                (Error::Input(_), "input") | (Error::Io(_), "io") | (Error::Run(_), "run")
            ));
            assert!(error.to_string().starts_with("primary preparation failure"));
            assert!(
                error
                    .to_string()
                    .contains("fixture uncertain preparation cleanup")
            );
            peer.join().unwrap();
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}

fn repeated_provider_finish_child(
    case: &str,
    root: &Path,
    config_path: &Path,
    config: BenchmarkConfig,
    cassette: &Path,
) {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let failed_finish = case.ends_with("failed-finish");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let _scope = entry_test_support::SharedProviderScope::new(
        root,
        Arc::new(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            if failed_finish {
                Err(Error::Run(
                    "fixture shared provider cleanup uncertain".into(),
                ))
            } else {
                Ok(())
            }
        }),
    );
    let mut output = Vec::new();
    let result = super::super::repeated::run(
        config_path,
        config,
        2,
        RunOptions {
            json: true,
            mode: Some(tape::Mode::Replay),
            cassette: Some(cassette.to_str().unwrap()),
            ..Default::default()
        },
        &mut output,
        &Mutex::new(BTreeSet::new()),
    );
    assert!(
        result.is_err(),
        "negative grade or cleanup failure must fail"
    );
    if failed_finish {
        assert!(
            result
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("provider cleanup")
        );
    }
    let rows: Vec<serde_json::Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let summaries: Vec<_> = rows
        .iter()
        .filter(|row| row.get("task").is_some())
        .collect();
    let expected = if failed_finish { 2 } else { 4 };
    assert_eq!(summaries.len(), expected, "{rows:?}");
    assert_eq!(calls.load(Ordering::SeqCst), expected);
    for row in summaries {
        assert_eq!(row["process_success"], true);
        assert_eq!(row["workload_success"], !case.contains("repeat-grader"));
        assert_eq!(
            row["benchmark_outcome"]["cleanup_confirmed"],
            !failed_finish
        );
        if failed_finish {
            assert_eq!(row["execution"]["batch"], 1);
        }
    }
    let campaign = PathBuf::from(rows[0]["path"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    assert_eq!(campaign.join("batch-2").exists(), !failed_finish);
    let final_record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(campaign.join("finished.json")).unwrap()).unwrap();
    assert_eq!(final_record["stopped"], failed_finish);
}

fn full_entry_child(case: &str) {
    if case.starts_with("bundle-publication-") {
        bundle_publication::run(case);
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Observer {
        started: AtomicUsize,
        completed: AtomicUsize,
        fail: bool,
        lifetime: Option<PathBuf>,
    }
    impl replica::CompletionObserver for Observer {
        fn starting(
            &self,
            _: replica::Identity,
            _: Option<&measurements::ExecutionCohort>,
        ) -> Result<(), Error> {
            self.started.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn completed(&self, _: replica::Identity, record: &Path) -> Result<(), Error> {
            if let Some(root) = &self.lifetime {
                entry_test_support::assert_workload_owner_live(root);
            }
            assert!(record.is_file());
            self.completed.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(Error::Io(
                    "private-observer-secret-must-not-be-exposed".into(),
                ))
            } else {
                Ok(())
            }
        }
    }
    let artifact_case = case.starts_with("artifact-loss-");
    let phase_case = case.starts_with("phase-");
    let phase_continuation = phase_case && (case.contains("-repeat-") || case.contains("-sweep-"));
    let provider_case = case.starts_with("provider-");
    let disposition_case = case.starts_with("disposition-");
    let publication_case = case.contains("publication-");
    let capture_case = case.starts_with("capture-");
    let capture_sweep = case.starts_with("capture-sweep-");
    let termination_case = case.starts_with("termination-");
    let fixture = Fixture::new();
    let root = fixture.0.canonicalize().unwrap();
    let project = project::ProjectConfig {
        tools_dir: "tools".into(),
        workloads_dir: "workloads".into(),
        workspace_dir: "workspace".into(),
        ..Default::default()
    };
    std::fs::create_dir(root.join(".asb")).unwrap();
    std::fs::write(
        root.join(".asb/project.toml"),
        toml::to_string(&project).unwrap(),
    )
    .unwrap();
    std::fs::create_dir(root.join("workloads")).unwrap();
    workloads::populate(&root.join("workloads")).unwrap();
    // These lifecycle fixtures exercise runner state, not a client protocol.
    let mini_swe_case = case == "capture-mini-swe-read-failure";
    let agent_id = if mini_swe_case || artifact_case || (phase_case && !phase_continuation) {
        "mini-swe"
    } else {
        "pi"
    };
    let agent = root.join(
        if mini_swe_case || artifact_case || (phase_case && !phase_continuation) {
            "tools/mini-swe/bin/mini"
        } else {
            "tools/pi/bin/pi"
        },
    );
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    let counter = root.join("agent-executions");
    let quoted_counter = counter.to_string_lossy().replace('\'', "'\\''");
    std::fs::write(&agent, format!("#!/bin/sh\nprintf 'called\\n' >> '{quoted_counter}'\nprintf '%s\\n' '{{\"type\":\"agent_end\",\"messages\":[{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"src/cli.py src/registry.py lookup\"}}]}}]}}'\n")).unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let cassette = root.join("cassette.jsonl");
    let body = serde_json::json!({"model":"fixture","tools":[{"type":"function","function":{"name":"fixture","parameters":{"type":"object"}}}],"messages":[]});
    let response_body = if capture_case && !case.contains("read-failure") {
        r#"{"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}"#
    } else {
        "{}"
    };
    let row = serde_json::json!({"request":{"method":"POST","url":"/v1/chat/completions","body":body.to_string()},"response":{"status":200,"headers":{"content-type":"application/json"},"body":response_body}});
    std::fs::write(&cassette, format!("{row}\n")).unwrap();
    if artifact_case
        || phase_case
        || provider_case
        || capture_case
        || termination_case
        || (disposition_case && !case.contains("run"))
    {
        let node = crate::tools::host_executable(Path::new("node")).unwrap();
        let answer = if case == "provider-false-failed-finish"
            || case.contains("false")
            || case.contains("repeat-grader")
        {
            "incorrect"
        } else {
            "src/cli.py src/registry.py lookup"
        };
        let response = serde_json::json!({"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"text","text":answer}]}]});
        let exit = if case == "provider-client-failed-finish" || case.contains("client") {
            7
        } else {
            0
        };
        let counter_js = serde_json::to_string(&counter).unwrap();
        let artifact_js = if mini_swe_case {
            "require('node:fs').writeFileSync(process.argv[process.argv.indexOf('--output')+1],JSON.stringify({trajectory_format:'mini-swe-agent-1.1',info:{model_stats:{api_calls:4}},messages:[{extra:{response:{usage:{prompt_tokens:11,completion_tokens:13}}}}]}));"
        } else {
            ""
        };
        let capture_js = if capture_case {
            format!(
                "require('node:fs').writeFileSync('fixture-file-grade','{}');{artifact_js}console.log(JSON.stringify({{usage:{{prompt_tokens:499,completion_tokens:500,total_tokens:999}}}}));process.stdout.write('x'.repeat(9*1024*1024));",
                !case.contains("false")
            )
        } else if termination_case {
            format!(
                "require('node:fs').writeFileSync('fixture-file-grade','{}');",
                !case.contains("false")
            )
        } else {
            String::new()
        };
        let ending = if termination_case {
            match case.rsplit('-').next().unwrap() {
                "code137" => "process.exitCode=137;",
                "code143" => "process.exitCode=143;",
                "signal9" => "process.kill(process.pid,'SIGKILL');",
                "signal15" => "process.kill(process.pid,'SIGTERM');",
                _ => "process.exitCode=0;",
            }
            .to_owned()
        } else {
            format!("process.exitCode={exit};")
        };
        std::fs::write(&agent, format!("#!{}\nrequire('node:fs').appendFileSync({counter_js},'called\\n');fetch(process.env.OPENAI_BASE_URL+'/chat/completions',{{method:'POST',headers:{{'content-type':'application/json'}},body:JSON.stringify({body})}}).then(async r=>{{if(!r.ok)throw new Error('fixture request failed');await r.text();{capture_js}console.log(JSON.stringify({response}));{ending}}}).catch(error=>{{console.error('fixture replay request failed:',error);process.exitCode=9;}});\n", node.display())).unwrap();
        if artifact_case || (phase_case && !phase_continuation) {
            let trajectory = serde_json::json!({"trajectory_format":"mini-swe-agent-1.1", "info":{"model_stats":{"api_calls":4}},
                "messages":[{"role":"assistant","content":answer,
                "extra":{"response":{"usage":{"prompt_tokens":13,"completion_tokens":21}}}}]});
            std::fs::write(&agent, format!("#!{}\nconst fs=require('node:fs');fs.appendFileSync({counter_js},'called\\n');const out=process.argv[process.argv.indexOf('--output')+1];fs.writeFileSync(out,JSON.stringify({trajectory}));fetch(process.env.OPENAI_BASE_URL+'/chat/completions',{{method:'POST',headers:{{'content-type':'application/json'}},body:JSON.stringify({body})}}).then(async r=>{{if(!r.ok)throw Error('fixture replay');await r.text();}}).catch(()=>{{process.exitCode=9;}});\n", node.display())).unwrap();
        }
    }
    if case == "provider-early-failed-finish" {
        std::fs::write(root.join("workspace"), "blocking fixture file").unwrap();
    }
    let mut config: BenchmarkConfig = toml::from_str(&format!("agent='{agent_id}'\nprovider='openrouter'\nmodel='fixture'\nworkload='repository-navigation'\nworkspace='workspace'\nlog_file='run.jsonl'\n[measurements]\nenabled={}\ndirectory='measurements'\ncollectors=[]\n", !capture_sweep)).unwrap();
    if case.starts_with("provider-repeat-") {
        config.concurrency = 2;
    }
    if artifact_case && case.ends_with("disabled") {
        config.measurements = None;
    }
    let config_path = root.join("benchmark.toml");
    std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    #[cfg(target_os = "linux")]
    if case.starts_with("termination-repeat-") {
        use std::sync::Arc;
        let _fault = case
            .contains("observation-fault")
            .then(crate::process_gate::observation_fault::SharedScope::enter);
        let finishes = Arc::new(AtomicUsize::new(0));
        let observed = finishes.clone();
        let _scope = entry_test_support::SharedProviderScope::new(
            &root,
            Arc::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        );
        let mut output = Vec::new();
        let result = super::super::repeated::run(
            &config_path,
            config,
            2,
            RunOptions {
                json: true,
                mode: Some(tape::Mode::Replay),
                cassette: Some(cassette.to_str().unwrap()),
                ..Default::default()
            },
            &mut output,
            &Mutex::new(BTreeSet::new()),
        );
        assert_eq!(result.is_ok(), !case.contains("false"), "{result:?}");
        let rows: Vec<serde_json::Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let summaries: Vec<_> = rows
            .iter()
            .filter(|row| row.get("task").is_some())
            .collect();
        assert_eq!(summaries.len(), 2, "both repeat batches must be admitted");
        for row in summaries {
            assert_eq!(row["process_success"], true);
            assert_eq!(row["workload_success"], !case.contains("false"));
            assert_eq!(
                row["process_termination"],
                if case.contains("observation-fault") {
                    serde_json::Value::Null
                } else {
                    serde_json::json!({"kind":"code","code":0})
                }
            );
            assert_eq!(row["benchmark_outcome"]["cleanup_confirmed"], true);
        }
        assert_eq!(finishes.load(Ordering::SeqCst), 2);
        assert_eq!(
            std::fs::read_to_string(counter).unwrap().lines().count(),
            2,
            "no retry"
        );
        let campaign = PathBuf::from(rows[0]["path"].as_str().unwrap())
            .parent()
            .unwrap()
            .to_owned();
        assert!(campaign.join("batch-2").is_dir());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(campaign.join("finished.json")).unwrap()
            )
            .unwrap()["stopped"],
            false
        );
        return;
    }
    if case.starts_with("provider-repeat-") {
        repeated_provider_finish_child(case, &root, &config_path, config, &cassette);
        return;
    }
    let blocked_log = artifact_case
        || case.starts_with("blocked-log")
        || (phase_case && case.contains("blocked-log"));
    if blocked_log {
        std::fs::create_dir(root.join("run.jsonl")).unwrap();
    }
    let calls = Rc::new(RefCell::new(Vec::new()));
    let event = if matches!(
        case,
        "execution-end"
            | "blocked-log"
            | "blocked-log-broken-output"
            | "observer-failure"
            | "observer-warning-writer"
    ) {
        LifecycleEvent::AgentExecuteEnd
    } else {
        LifecycleEvent::RunFinishing
    };
    let _capture_scope = case
        .contains("read-failure")
        .then(super::capture::test_support::Scope::read_failure);
    let _scope = if phase_case {
        entry_test_support::Scope::phase(&root, calls.clone(), case)
    } else if artifact_case {
        entry_test_support::Scope::artifact(&root, calls.clone(), !case.ends_with("disabled"))
    } else if disposition_case || capture_case || termination_case {
        entry_test_support::Scope::disposition(&root, calls.clone(), case)
    } else if provider_case {
        entry_test_support::Scope::provider(
            &root,
            calls.clone(),
            !case.ends_with("success"),
            case.contains("native"),
        )
    } else {
        entry_test_support::Scope::new(&root, event, calls.clone())
    };
    if capture_sweep {
        check_unmeasured_capture_sweep(case, &root, &cassette, &counter, &calls);
        return;
    }
    let observer = Observer {
        started: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
        fail: case.starts_with("observer-"),
        lifetime: case.contains("owner-lifetime").then(|| root.clone()),
    };
    struct Presentation {
        bytes: Vec<u8>,
        fail_result: bool,
        fail_warning: bool,
        fail_stopped_warning: bool,
        summary_done: bool,
        writes: usize,
        failures: usize,
        lifetime: Option<Rc<RefCell<Vec<&'static str>>>>,
    }
    impl Write for Presentation {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(calls) = &self.lifetime {
                assert!(
                    !calls.borrow().contains(&"workload-drop"),
                    "owner must survive final presentation"
                );
            }
            self.writes += 1;
            if self.fail_result
                || (self.fail_warning && self.summary_done)
                || (self.fail_stopped_warning
                    && bytes.starts_with(b"Warning: workload resources were stopped and preserved"))
            {
                self.failures += 1;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "combined fixture presentation closed",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            self.summary_done |= bytes.contains(&b'\n');
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl PhaseOutput for Presentation {
        fn observed_bytes(&self) -> &[u8] {
            &self.bytes
        }
    }
    let mut output = Presentation {
        bytes: Vec::new(),
        fail_result: case == "blocked-log-broken-output"
            || (disposition_case && case.contains("output")),
        fail_warning: case == "observer-warning-writer",
        fail_stopped_warning: case.starts_with("phase-warning-"),
        summary_done: false,
        writes: 0,
        failures: 0,
        lifetime: case.contains("owner-lifetime").then(|| calls.clone()),
    };
    if phase_case && (case.contains("-repeat-") || case.contains("-sweep-")) {
        std::mem::forget(fixture);
        check_phase_continuation(
            case,
            &root,
            config,
            &cassette,
            &counter,
            &calls,
            &mut output,
        );
        assert_eq!(
            output.failures, 1,
            "only the post-grade stopped warning fails"
        );
        return;
    }
    let _publication_fault = publication_case.then(|| publication::prepare(case, &root));
    let cohort = measurements::ExecutionCohort {
        id: workloads::TaskIdentity::local("execution-cohort", None).attempt_id,
        selected_replicas: 1,
        admitted_slots: vec![1],
        dispatch: None,
    };
    let options = RunOptions {
        json: !(case.starts_with("phase-warning-")
            || (disposition_case && case.contains("output"))),
        show_output: disposition_case && case.contains("output"),
        mode: Some(tape::Mode::Replay),
        cassette: Some(cassette.to_str().unwrap()),
        on_completion: Some(&observer),
        execution_cohort: (provider_case || disposition_case || capture_case || termination_case)
            .then_some(&cohort),
        measurement_dir: (case == "phase-removed-cwd").then_some("../measurements"),
        ..Default::default()
    };
    if case == "phase-removed-cwd" {
        let cwd = root.join("empty-cwd");
        std::fs::create_dir(&cwd).unwrap();
        // This function runs only in the isolated libtest subprocess.
        std::env::set_current_dir(cwd).unwrap();
    }
    #[cfg(target_os = "linux")]
    let _observation_fault = case
        .contains("observation-fault")
        .then(crate::process_gate::observation_fault::Scope::enter);
    let result = run_mode(
        &config_path,
        config,
        options,
        &mut output,
        &Mutex::new(BTreeSet::new()),
    );
    if phase_case {
        // Preserve the owned red fixture and raw/frozen artifact evidence even
        // when the publication assertions fail. No host namespace is touched.
        std::mem::forget(fixture);
        check_phase_completion(
            case,
            &root,
            result,
            &calls,
            &counter,
            observer.completed.load(Ordering::SeqCst),
            output.failures,
        );
        return;
    }
    if termination_case {
        let summary: serde_json::Value = serde_json::from_slice(&output.bytes).unwrap();
        let measured: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("measurements/run.json")).unwrap())
                .unwrap();
        let log: serde_json::Value = serde_json::from_str(
            std::fs::read_to_string(root.join("run.jsonl"))
                .unwrap()
                .lines()
                .last()
                .unwrap(),
        )
        .unwrap();
        let exact = if case.contains("observation-fault") {
            serde_json::Value::Null
        } else if case.ends_with("signal9") {
            serde_json::json!({"kind":"signal","signal":9})
        } else if case.ends_with("signal15") {
            serde_json::json!({"kind":"signal","signal":15})
        } else {
            serde_json::json!({"kind":"code","code":if case.ends_with("code137") {137} else if case.ends_with("code143") {143} else {0}})
        };
        let process_ok = case.ends_with("code0") || case.contains("observation-fault");
        // These actual entry cases deliberately supply a historical cohort
        // without dispatch. The consuming pipeline must not retrofit C1/P1
        // into persisted plans/hashes while reporting unknown dispatch.
        let saved_config: BenchmarkConfig =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        let saved_policy: crate::planning::ResolvedPlan =
            serde_json::from_value(summary["planning"].clone()).unwrap();
        let effective: crate::planning::EffectiveBudgets =
            serde_json::from_value(summary["effective_budgets"].clone()).unwrap();
        let legacy_fingerprint = crate::planning::protocol_fingerprint_with_admission(
            &saved_config,
            &saved_policy,
            effective,
            1,
            None,
        )
        .unwrap();
        let inferred_fingerprint = crate::planning::protocol_fingerprint_with_dispatch(
            &saved_config,
            &saved_policy,
            effective,
            1,
            None,
            Some(crate::planning::Dispatch::new(1, 1).unwrap()),
        )
        .unwrap();
        assert_ne!(legacy_fingerprint, inferred_fingerprint);
        let plan: serde_json::Value = serde_json::from_slice(
            &std::fs::read(summary["execution_plan"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert!(plan.get("dispatch").is_none(), "{case}: {plan}");
        for record in [&summary, &log, &measured] {
            assert_eq!(
                record["execution_cohort"],
                serde_json::to_value(&cohort).unwrap(),
                "{case}"
            );
            assert!(
                record["execution_cohort"].get("dispatch").is_none(),
                "{case}"
            );
            assert_eq!(
                record["execution_plan_fingerprint"],
                legacy_fingerprint.as_str(),
                "{case}"
            );
            assert_eq!(record["process_termination"], exact, "{case}");
            assert_eq!(record["process_success"], process_ok, "{case}");
            assert_eq!(
                record["benchmark_outcome"]["cleanup_confirmed"], true,
                "{case}"
            );
            assert_eq!(
                record["workload_success"],
                if case.contains("pending") {
                    serde_json::Value::Null
                } else {
                    serde_json::json!(!case.contains("false"))
                },
                "{case}"
            );
            assert_eq!(
                record.get("process_termination_diagnostic").is_some(),
                case.contains("observation-fault"),
                "{case}"
            );
        }
        assert_eq!(
            result.is_ok(),
            process_ok && !case.contains("false"),
            "{case}: {result:?}"
        );
        assert_eq!(
            observer.completed.load(Ordering::SeqCst),
            1,
            "optional evidence does not change private outcome publication: {case}"
        );
        assert_eq!(
            calls.borrow().as_slice(),
            ["evaluate", "finish", "provider-finish"]
        );
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "called\n");
        let completions: Vec<_> = std::fs::read_dir(root.join(".asb-completions"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(completions.len(), 1);
        let private: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&completions[0]).unwrap()).unwrap();
        assert_eq!(private["summary"]["process_termination"], exact);
        assert_eq!(private["summary"], summary);
        let report =
            crate::compare::generate(config_path.to_str().unwrap(), &mut Vec::new()).unwrap();
        let comparison = report.with_file_name("comparison.json");
        let original_json = std::fs::read(&comparison).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&original_json).unwrap();
        assert_eq!(document["benchmarks"][0]["process_termination"], exact);
        let original_html = std::fs::read(&report).unwrap();
        assert!(
            String::from_utf8_lossy(&original_html).contains("principal termination")
                || String::from_utf8_lossy(&original_html).contains("principal exit code")
                || String::from_utf8_lossy(&original_html).contains("principal signal")
        );
        if case == "termination-true-code137" {
            for bad in [
                serde_json::json!({"kind":"signal","signal":9}),
                serde_json::json!({"kind":"signal","signal":0}),
            ] {
                let mut changed = measured.clone();
                changed["process_termination"] = bad;
                let bytes = serde_json::to_vec(&changed).unwrap();
                std::fs::write(root.join("measurements/run.json"), &bytes).unwrap();
                assert!(
                    crate::compare::generate(config_path.to_str().unwrap(), &mut Vec::new())
                        .is_err()
                );
                assert_eq!(std::fs::read(&comparison).unwrap(), original_json);
                assert_eq!(std::fs::read(&report).unwrap(), original_html);
                assert_eq!(
                    std::fs::read(root.join("measurements/run.json")).unwrap(),
                    bytes
                );
            }
        }
        return;
    }
    if artifact_case {
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("run log publication failed after task evaluation"),
            "{error}"
        );
        assert!(error.contains("retained frozen agent evidence"), "{error}");
        if !case.ends_with("disabled") {
            assert!(error.contains("measurement finalization failed"), "{error}");
            assert!(
                error.find("run log publication failed").unwrap()
                    < error.find("measurement finalization failed").unwrap()
            );
        }
        let summary: serde_json::Value = serde_json::from_slice(&output.bytes).unwrap();
        assert_eq!(summary["workload_success"], true);
        assert_eq!(summary["token_usage"]["total_tokens"], 34);
        assert_eq!(summary["steps"], 4);
        assert_eq!(summary["benchmark_outcome"]["cleanup_confirmed"], true);
        assert_eq!(
            calls.borrow().as_slice(),
            ["evaluate", "finish", "provider-finish"]
        );
        assert_eq!(std::fs::read_to_string(counter).unwrap(), "called\n");
        assert_eq!(observer.completed.load(Ordering::SeqCst), 0);
        assert!(!root.join(".asb-completions").exists());
        let runs = replica::runtime_path(&root.join("workspace")).join("runs");
        let attempts: Vec<_> = std::fs::read_dir(runs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(attempts.len(), 1);
        let frozen = attempts[0].join("mini-swe/frozen-trajectory.json");
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&frozen).unwrap()).unwrap();
        assert_eq!(raw["info"]["model_stats"]["api_calls"], 4);
        assert_eq!(
            raw["messages"][0]["extra"]["response"]["usage"]["prompt_tokens"],
            13
        );
        assert_eq!(
            raw["messages"][0]["content"],
            "src/cli.py src/registry.py lookup"
        );
        assert!(!attempts[0].join("mini-swe/trajectory.json").exists());
        assert_eq!(
            output.bytes.iter().filter(|byte| **byte == b'\n').count(),
            1,
            "one summary, never append a corrected duplicate"
        );
        return;
    }
    if publication_case {
        publication::check(
            case,
            &root,
            result,
            &calls,
            &output.bytes,
            observer.completed.load(Ordering::SeqCst),
        );
        assert_eq!(std::fs::read_to_string(counter).unwrap(), "called\n");
        return;
    }
    if capture_case {
        if !case.contains("read-failure") && !case.contains("false") {
            result.expect("a sealed terminal guest result may outlive its bounded presentation");
            assert_eq!(std::fs::read_to_string(counter).unwrap(), "called\n");
            assert_eq!(
                calls.borrow().as_slice(),
                ["evaluate", "finish", "provider-finish"]
            );
            return;
        }
        let error = result.unwrap_err().to_string();
        assert!(error.contains(if case.contains("read-failure") {
            "capture failed"
        } else if case.contains("false") {
            "unresolved"
        } else {
            "capture limit"
        }));
        assert_eq!(std::fs::read_to_string(counter).unwrap(), "called\n");
        assert_eq!(
            calls.borrow().as_slice(),
            ["evaluate", "finish", "provider-finish"]
        );
        let log: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("run.jsonl")).unwrap()).unwrap();
        let measured: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("measurements/run.json")).unwrap())
                .unwrap();
        for record in [&log, &measured] {
            assert_eq!(record["status"], "failed");
            assert_eq!(
                record["failure_kind"],
                if case.contains("false") {
                    "workload_check_failed"
                } else {
                    "run_error"
                }
            );
            assert_eq!(
                record["benchmark_outcome"]["evaluation"]["status"],
                "completed"
            );
            if case.contains("native") {
                assert!(record["workload_success"].is_null());
                assert_eq!(
                    record["benchmark_outcome"]["evaluation"]["rewards"]["accuracy"],
                    0.5
                );
            } else {
                assert_eq!(record["workload_success"], !case.contains("false"));
            }
            if mini_swe_case {
                assert_eq!(record["token_usage"]["total_tokens"], 24);
                assert_eq!(record["steps"], 4);
            } else if case.contains("read-failure") {
                assert!(record["token_usage"]["total_tokens"].is_null());
                assert!(record["steps"].is_null());
            } else {
                assert_eq!(record["token_usage"]["total_tokens"], 7);
                assert!(
                    record["token_usage"]["source"]
                        .as_str()
                        .unwrap()
                        .contains("cassette")
                );
            }
        }
        if case.contains("false") {
            return;
        }
        assert_eq!(observer.completed.load(Ordering::SeqCst), 0);
        assert!(!root.join(".asb-completions").exists());
        let lines = std::str::from_utf8(&output.bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].get("status").is_none());
        assert_eq!(lines[1]["event"], "continuation_warning");
        let report =
            crate::compare::generate(config_path.to_str().unwrap(), &mut Vec::new()).unwrap();
        let comparison: serde_json::Value = serde_json::from_slice(
            &std::fs::read(report.with_file_name("comparison.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            comparison["benchmarks"][0]["benchmark_outcome"],
            log["benchmark_outcome"]
        );
        return;
    }
    if disposition_case {
        check_full_entry_disposition(
            case,
            &root,
            result,
            &calls,
            &output.bytes,
            observer.completed.load(Ordering::SeqCst),
        );
        assert_eq!(std::fs::read_to_string(counter).unwrap(), "called\n");
        return;
    }
    if provider_case {
        let error = result.err().map(|error| error.to_string());
        assert_eq!(error.is_none(), case.ends_with("success"), "{error:?}");
        assert_eq!(
            calls
                .borrow()
                .iter()
                .filter(|call| **call == "provider-finish")
                .count(),
            1
        );
        assert!(!calls.borrow().contains(&"provider-fallback"));
        if case == "provider-early-failed-finish" {
            let error = error.unwrap();
            assert!(error.starts_with("cannot create workspace"), "{error}");
            assert!(error.contains("fixture provider cleanup uncertain"));
            assert_eq!(calls.borrow().as_slice(), ["provider-finish"]);
            assert!(!counter.exists());
            assert!(!root.join(".asb-completions").exists());
            return;
        }
        assert_eq!(
            calls.borrow().as_slice(),
            ["evaluate", "finish", "provider-finish"]
        );
        assert_eq!(
            observer.completed.load(Ordering::SeqCst),
            usize::from(case.ends_with("success"))
        );
        assert_eq!(
            root.join(".asb-completions").exists(),
            case.ends_with("success")
        );
        let log: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("run.jsonl")).unwrap()).unwrap();
        let summary: serde_json::Value = serde_json::from_slice(&output.bytes).unwrap();
        let measured: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("measurements/run.json")).unwrap())
                .unwrap();
        let plan: serde_json::Value = serde_json::from_slice(
            &std::fs::read(log["execution_plan"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(plan["policy"], log["planning"]);
        assert_eq!(plan["effective_controller_caps"], log["effective_budgets"]);
        assert!(!plan["task"].is_null());
        for record in [&log, &summary] {
            assert_eq!(record["planning"], log["planning"]);
            assert_eq!(record["effective_budgets"], log["effective_budgets"]);
            assert_eq!(record["execution_plan"], log["execution_plan"]);
        }
        for record in [&log, &summary, &measured] {
            assert_eq!(
                record["execution_plan_fingerprint"],
                log["execution_plan_fingerprint"]
            );
            assert!(
                record["execution_plan_fingerprint"]
                    .as_str()
                    .unwrap()
                    .starts_with("asb-protocol-v1:")
            );
            if case.contains("native") {
                assert!(record["workload_success"].is_null());
                assert!(record["success_rate"].is_null());
                assert_eq!(
                    record["benchmark_outcome"]["evaluation"]["status"],
                    "completed"
                );
                assert!(record["benchmark_outcome"]["evaluation"]["resolution"].is_null());
                assert_eq!(
                    record["benchmark_outcome"]["evaluation"]["rewards"]["accuracy"],
                    0.5
                );
            } else {
                assert_eq!(
                    record["workload_success"],
                    case != "provider-false-failed-finish"
                );
            }
            assert_eq!(
                record["benchmark_outcome"]["cleanup_confirmed"],
                case.ends_with("success")
            );
            assert_eq!(
                record["benchmark_outcome"]["execution_success"],
                case != "provider-client-failed-finish"
            );
            assert_eq!(
                record["process_success"],
                case != "provider-client-failed-finish"
            );
        }
        if case.contains("native") {
            // Render the actual attempt's saved measurement/log contract. A
            // completed numeric reward must retain independent cleanup facts.
            let report =
                crate::compare::generate(config_path.to_str().unwrap(), &mut Vec::new()).unwrap();
            let comparison: serde_json::Value = serde_json::from_slice(
                &std::fs::read(report.with_file_name("comparison.json")).unwrap(),
            )
            .unwrap();
            let row = &comparison["benchmarks"][0];
            assert_eq!(
                row["benchmark_outcome"]["evaluation"]["rewards"]["accuracy"],
                0.5
            );
            assert!(row["benchmark_outcome"]["evaluation"]["resolution"].is_null());
            assert_eq!(row["benchmark_outcome"]["execution_success"], true);
            assert_eq!(
                row["benchmark_outcome"]["cleanup_confirmed"],
                case.ends_with("success")
            );
            let html = std::fs::read_to_string(report).unwrap();
            if case.ends_with("success") {
                assert_eq!(row["status"], "completed");
                assert!(!html.contains("fixture provider cleanup uncertain"));
                assert!(!row["status"].as_str().unwrap().contains("failed"));
            } else {
                assert!(
                    row["status"]
                        .as_str()
                        .unwrap()
                        .contains("fixture provider cleanup uncertain")
                );
                assert!(html.contains("fixture provider cleanup uncertain"));
            }
        }
        if let Some(error) = error {
            assert!(
                error.contains("fixture provider cleanup uncertain"),
                "{error}"
            );
            if case == "provider-client-failed-finish" {
                assert_eq!(summary["failure_kind"], "agent_failure");
                assert!(
                    error
                        .find(summary["failure_reason"].as_str().unwrap())
                        .unwrap()
                        < error.find("provider cleanup failed").unwrap()
                );
            } else if case == "provider-false-failed-finish" {
                assert_eq!(summary["failure_kind"], "workload_check_failed");
            } else {
                assert_eq!(summary["failure_kind"], "provider_cleanup_error");
            }
        }
        return;
    }
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("replay consumed 0 of 1"),
        "primary proxy validation must not be replaced: {error}"
    );
    if matches!(case, "run-finishing" | "blocked-log-and-measurements") {
        assert!(
            error.contains("measurement finalization failed after task evaluation"),
            "secondary finalization diagnostic must remain available: {error}"
        );
    }
    if blocked_log {
        assert!(
            error.contains("run log publication failed after task evaluation"),
            "{error}"
        );
        assert!(error.contains("regular file"), "{error}");
        assert!(
            error.contains(root.join("run.jsonl").to_str().unwrap()),
            "{error}"
        );
        assert!(
            error.find("replay consumed 0 of 1").unwrap()
                < error.find("run log publication failed").unwrap(),
            "primary error must precede ancillary publication failure: {error}"
        );
        if case == "blocked-log-and-measurements" {
            assert!(
                error.find("run log publication failed").unwrap()
                    < error.find("measurement finalization failed").unwrap(),
                "both independent publication failures must be retained: {error}"
            );
        }
    }
    if matches!(
        case,
        "blocked-log-broken-output" | "observer-warning-writer"
    ) {
        assert!(
            error.contains("combined fixture presentation closed"),
            "{error}"
        );
        assert!(
            error.contains("result or continuation diagnostic output also failed"),
            "{error}"
        );
        assert_eq!(
            output.failures, 1,
            "never repeat output after the writer fails"
        );
        if case == "blocked-log-broken-output" {
            assert_eq!(output.writes, 1);
            assert!(output.bytes.is_empty());
            assert!(
                error.find("run log publication failed").unwrap()
                    < error
                        .find("result or continuation diagnostic output also failed")
                        .unwrap()
            );
        }
    }
    assert_eq!(std::fs::read_to_string(counter).unwrap(), "called\n");
    assert_eq!(calls.borrow().as_slice(), ["evaluate", "finish"]);
    assert_eq!(observer.started.load(Ordering::SeqCst), 1);
    let complete = case == "execution-end" || observer.fail;
    assert_eq!(
        observer.completed.load(Ordering::SeqCst),
        usize::from(complete)
    );
    assert_eq!(root.join(".asb-completions").exists(), complete);
    let log: Option<serde_json::Value> = (!blocked_log)
        .then(|| serde_json::from_slice(&std::fs::read(root.join("run.jsonl")).unwrap()).unwrap());
    let lines: Vec<serde_json::Value> = std::str::from_utf8(&output.bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let summary = lines.iter().find(|line| line.get("event").is_none());
    assert_eq!(summary.is_some(), case != "blocked-log-broken-output");
    for value in log.iter().chain(summary) {
        assert_eq!(
            value["workload_success"], true,
            "real bundled evaluator grade is independent of proxy/collector failure"
        );
        assert_eq!(value["process_success"], true);
        assert_eq!(value["success_rate"], 1.0);
        assert_eq!(value["failure_kind"], "run_error");
    }
    let measured: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("measurements/run.json")).unwrap())
            .unwrap();
    assert_eq!(measured["workload_success"], true);
    assert_eq!(measured["process_success"], true);
    assert_eq!(measured["success_rate"], 1.0);
    assert!(
        error.starts_with(measured["failure_reason"].as_str().unwrap()),
        "original primary must remain the returned error prefix: {error}"
    );
    if let Some(summary) = summary {
        assert_eq!(measured["failure_reason"], summary["failure_reason"]);
    }
    let warnings: Vec<_> = lines
        .iter()
        .filter(|line| line["event"] == "continuation_warning")
        .collect();
    if case == "observer-failure" {
        assert_eq!(
            lines.len(),
            2,
            "one unchanged summary then one typed warning"
        );
        assert!(lines[0].get("event").is_none());
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["code"], "replica_receipt_unavailable");
        assert_eq!(warnings[0]["path_role"], "source_outcome");
        let source = Path::new(warnings[0]["path"].as_str().unwrap());
        assert!(source.is_file());
        assert_eq!(
            source.parent(),
            Some(root.join(".asb-completions").as_path())
        );
    } else {
        assert!(warnings.is_empty());
    }
    assert!(!error.contains("private-observer-secret-must-not-be-exposed"));
    assert!(
        !String::from_utf8_lossy(&output.bytes)
            .contains("private-observer-secret-must-not-be-exposed")
    );
    if blocked_log {
        assert!(
            root.join("run.jsonl").is_dir(),
            "never replace the blocking user directory"
        );
    }
}

fn check_phase_completion(
    case: &str,
    root: &Path,
    result: Result<(), Error>,
    calls: &Rc<RefCell<Vec<&'static str>>>,
    counter: &Path,
    completed: usize,
    output_failures: usize,
) {
    if case.contains("runtime-error") {
        let error = result.unwrap_err().to_string();
        assert!(
            error.starts_with("fixture started execution failed"),
            "{error}"
        );
        if case.contains("provider") {
            assert!(error.contains("provider cleanup failed"), "{error}");
        }
        assert_eq!(
            calls.borrow().as_slice(),
            ["execute-entered", "workload-drop", "provider-finish"]
        );
        assert!(!counter.exists());
        assert!(!root.join("workspace/phase-grade.json").exists());
        assert!(!root.join("run.jsonl").exists());
        assert!(!root.join("measurements/run.json").exists());
        assert!(!root.join(".asb-completions").exists());
        assert_eq!(completed, 0);
        return;
    }
    if case.contains("prepare-workload-end") {
        let error = result.unwrap_err().to_string();
        assert!(error.contains("PhaseEnd(WorkloadPreparation)"), "{error}");
        if case.contains("provider") {
            assert!(error.contains("provider cleanup failed"), "{error}");
        }
        assert_eq!(
            calls.borrow().as_slice(),
            ["cancel-prepared", "provider-finish"]
        );
        assert!(!counter.exists());
        assert!(!root.join("workspace/phase-grade.json").exists());
        assert!(!root.join("run.jsonl").exists());
        assert!(!root.join("measurements/run.json").exists());
        assert!(!root.join(".asb-completions").exists());
        assert_eq!(completed, 0);
        assert_eq!(output_failures, 0);
        return;
    }
    let grade: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("workspace/phase-grade.json")).unwrap())
            .unwrap();
    assert_eq!(grade["status"], "completed");
    if case.contains("native") {
        assert!(grade["resolution"].is_null());
        assert_eq!(grade["rewards"]["accuracy"], 0.5);
    } else {
        assert_eq!(grade["resolution"], !case.contains("false"));
    }
    let expected: &[&str] = if case.contains("owner-lifetime") {
        &["evaluate", "finish", "provider-finish", "workload-drop"]
    } else {
        &["evaluate", "finish", "provider-finish"]
    };
    assert_eq!(calls.borrow().as_slice(), expected);
    assert_eq!(std::fs::read_to_string(counter).unwrap(), "called\n");
    assert_eq!(
        output_failures,
        usize::from(case.starts_with("phase-warning-"))
    );
    let runs = replica::runtime_path(&root.join("workspace")).join("runs");
    let attempts: Vec<_> = std::fs::read_dir(runs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(attempts.len(), 1);
    let frozen = attempts[0].join("mini-swe/frozen-trajectory.json");
    let log = root.join("run.jsonl");
    let measured = root.join("measurements/run.json");
    eprintln!(
        "phase completion evidence: case={case} root={} grade={grade} calls={:?} result={result:?} log={} measurements={} handoff={} raw={}",
        root.display(),
        calls.borrow(),
        log.is_file(),
        measured.is_file(),
        frozen.is_file(),
        attempts[0].join("mini-swe/trajectory.json").is_file()
    );
    let blocked_log = case.contains("blocked-log");
    assert!(
        (log.is_file() || (blocked_log && log.is_dir())) && measured.is_file() && frozen.is_file(),
        "post-grade failure must not bypass independent log/measurement publication or artifact handoff"
    );
    for path in [(!blocked_log).then_some(log), Some(measured)]
        .into_iter()
        .flatten()
    {
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(
            record["process_termination"],
            if case.contains("observation-fault") {
                serde_json::Value::Null
            } else {
                serde_json::json!({"kind":"code","code":0})
            }
        );
        assert_eq!(
            record.get("process_termination_diagnostic").is_some(),
            case.contains("observation-fault")
        );
        assert_eq!(record["benchmark_outcome"]["evaluation"], grade);
        assert_eq!(record["workload_success"], grade["resolution"]);
        assert_eq!(record["token_usage"]["total_tokens"], 34);
        assert_eq!(record["steps"], 4);
        assert_eq!(
            record["workload_resource_disposition"],
            if case.starts_with("phase-warning-") {
                "stopped-preserved"
            } else {
                "not-required"
            }
        );
        if case.starts_with("phase-warning-") {
            assert!(
                record["stderr"]
                    .as_str()
                    .is_none_or(|stderr| stderr.contains("ASB presentation error"))
            );
        }
    }
    if case.starts_with("phase-warning-") {
        assert!(matches!(&result, Err(Error::Io(_))), "{result:?}");
        let error = result.as_ref().unwrap_err().to_string();
        assert!(
            error.contains("combined fixture presentation closed"),
            "{error}"
        );
        if case.contains("false") {
            assert!(
                error.starts_with(&format!(
                    "workload check failed: {}",
                    grade["reason"].as_str().unwrap()
                )),
                "{error}"
            );
        }
        if blocked_log {
            assert!(error.contains("run log publication failed"), "{error}");
            assert!(error.contains("measurement finalization failed"), "{error}");
            assert!(error.contains("provider cleanup failed"), "{error}");
            assert!(
                error.find("run log publication failed").unwrap()
                    < error.find("measurement finalization failed").unwrap()
            );
        }
    } else {
        assert!(result.is_ok(), "{result:?}");
    }
    let expect_private_outcome = !case.contains("provider") && !blocked_log;
    assert_eq!(completed, usize::from(expect_private_outcome));
    if !expect_private_outcome {
        assert!(!root.join(".asb-completions").exists());
        return;
    }
    let outcomes: Vec<_> = std::fs::read_dir(root.join(".asb-completions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(outcomes.len(), 1);
    let outcome: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&outcomes[0]).unwrap()).unwrap();
    assert_eq!(outcome["scope"], "task-outcome-not-yet-a-resume-receipt");
    assert_eq!(outcome["summary"]["benchmark_outcome"]["evaluation"], grade);
    assert_eq!(
        outcome["summary"]["process_termination"],
        if case.contains("observation-fault") {
            serde_json::Value::Null
        } else {
            serde_json::json!({"kind":"code","code":0})
        }
    );
    assert_eq!(
        outcome["summary"]
            .get("process_termination_diagnostic")
            .is_some(),
        case.contains("observation-fault")
    );
    // A reporting-only outcome is not a sweep checkpoint/resume receipt. Actual
    // repeated/sweep validation remains a separate matrix for the repair.
    assert!(!root.join(".checkpoint").exists());
}

trait PhaseOutput: Write {
    fn observed_bytes(&self) -> &[u8];
}

fn check_phase_continuation(
    case: &str,
    root: &Path,
    config: BenchmarkConfig,
    cassette: &Path,
    counter: &Path,
    calls: &Rc<RefCell<Vec<&'static str>>>,
    output: &mut dyn PhaseOutput,
) {
    let config_path = root.join("benchmark.toml");
    let log;
    if case.contains("-repeat-") {
        let error = super::super::repeated::run(
            &config_path,
            config,
            2,
            RunOptions {
                mode: Some(tape::Mode::Replay),
                cassette: Some(cassette.to_str().unwrap()),
                ..Default::default()
            },
            output,
            &Mutex::new(BTreeSet::new()),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("stopped further admission"), "{error}");
        let campaigns: Vec<_> = std::fs::read_dir(root.join("results/asb-executions"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(campaigns.len(), 1);
        let campaign = &campaigns[0];
        assert!(!campaign.join("batch-2").exists());
        let finished: serde_json::Value =
            serde_json::from_slice(&std::fs::read(campaign.join("finished.json")).unwrap())
                .unwrap();
        assert_eq!(finished["stopped"], true);
        log = campaign.join("batch-1/run.jsonl");
        let private: Vec<_> = std::fs::read_dir(campaign.join("batch-1/.asb-completions"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(private.len(), 1, "reporting-only outcome still published");
        let outcome: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&private[0]).unwrap()).unwrap();
        assert_eq!(outcome["scope"], "task-outcome-not-yet-a-resume-receipt");
    } else {
        let manifest = root.join("sweep.toml");
        std::fs::write(&manifest, "configs=['benchmark.toml']\n").unwrap();
        let tapes = root.join("tapes");
        let directory = tapes.join("000-benchmark/cassettes");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::copy(
            cassette,
            directory.join(default_cassette(&config_path, &config).file_name().unwrap()),
        )
        .unwrap();
        let options = crate::sweep::Options {
            mode: Some(tape::Mode::Replay),
            cassette_root: Some(&tapes),
            ..Default::default()
        };
        let error = crate::sweep::run_with_options(manifest.to_str().unwrap(), options, output)
            .unwrap_err()
            .to_string();
        assert!(error.contains("presentation closed"), "{error}");
        let results = root.join("sweep-results");
        log = results.join("000-benchmark/run.log");
        let before = std::fs::read(&log).unwrap();
        // Actual existing validation accepts a completed, confirmed grade even
        // after presentation failure. This is not permission to run another client.
        assert!(
            results.join(".checkpoint/completed/000000.json").is_file(),
            "sweep continuation evidence: {}",
            String::from_utf8_lossy(output.observed_bytes())
        );
        let mut reused_output = Vec::new();
        let historical_failure = crate::sweep::run_with_options(
            manifest.to_str().unwrap(),
            crate::sweep::Options {
                resume: true,
                ..options
            },
            &mut reused_output,
        )
        .unwrap_err()
        .to_string();
        assert!(
            historical_failure.contains("reused benchmark outcome"),
            "{historical_failure}"
        );
        assert!(
            String::from_utf8(reused_output)
                .unwrap()
                .contains("Reused completed task")
        );
        assert_eq!(std::fs::read(&log).unwrap(), before);
    }
    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(&log).unwrap()).unwrap();
    assert_eq!(record["status"], "failed");
    assert_eq!(record["workload_resource_disposition"], "stopped-preserved");
    assert_eq!(record["benchmark_outcome"]["cleanup_confirmed"], true);
    assert_eq!(
        record["benchmark_outcome"]["evaluation"]["status"],
        "completed"
    );
    if case.contains("native") {
        assert!(record["workload_success"].is_null());
        assert_eq!(
            record["benchmark_outcome"]["evaluation"]["rewards"]["accuracy"],
            0.5
        );
    } else {
        assert_eq!(record["workload_success"], !case.contains("false"));
    }
    assert_eq!(std::fs::read_to_string(counter).unwrap(), "called\n");
    assert_eq!(
        calls.borrow().as_slice(),
        ["evaluate", "finish", "provider-finish"]
    );
}

fn check_unmeasured_capture_sweep(
    case: &str,
    root: &Path,
    cassette: &Path,
    counter: &Path,
    calls: &Rc<RefCell<Vec<&'static str>>>,
) {
    let manifest = root.join("sweep.toml");
    std::fs::write(&manifest, "configs=['benchmark.toml']\n").unwrap();
    let tapes = root.join("tapes");
    let tape_directory = tapes.join("000-benchmark/cassettes");
    std::fs::create_dir_all(&tape_directory).unwrap();
    std::fs::copy(
        cassette,
        tape_directory.join("benchmark-pi-repository-navigation-fixture.jsonl"),
    )
    .unwrap();
    let options = crate::sweep::Options {
        json: true,
        mode: Some(tape::Mode::Replay),
        cassette_root: Some(&tapes),
        ..Default::default()
    };
    let result =
        crate::sweep::run_with_options(manifest.to_str().unwrap(), options, &mut Vec::new());
    if !case.contains("read-failure") && !case.contains("false") {
        result.expect("a sealed terminal guest result may outlive its bounded presentation");
        assert_eq!(std::fs::read_to_string(counter).unwrap(), "called\n");
        return;
    }
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains(if case.contains("false") {
            "unresolved"
        } else {
            "capture limit"
        }),
        "{error}"
    );
    let results = root.join("sweep-results");
    let directory = results.join("000-benchmark");
    let log_bytes = std::fs::read(directory.join("run.log")).unwrap();
    let log: serde_json::Value = serde_json::from_slice(&log_bytes).unwrap();
    assert_eq!(log["status"], "failed");
    assert_eq!(
        log["failure_kind"],
        if case.contains("false") {
            "workload_check_failed"
        } else {
            "run_error"
        }
    );
    assert_eq!(log["workload_resource_disposition"], "not-required");
    assert!(log["workload_artifacts"].is_array());
    assert_eq!(
        log["benchmark_outcome"]["evaluation"]["status"],
        "completed"
    );
    if case.contains("native") {
        assert!(log["workload_success"].is_null());
        assert!(log["benchmark_outcome"]["evaluation"]["resolution"].is_null());
        assert_eq!(
            log["benchmark_outcome"]["evaluation"]["rewards"]["accuracy"],
            0.5
        );
    } else {
        assert_eq!(log["workload_success"], !case.contains("false"));
    }
    if case.contains("false") {
        return;
    }
    assert!(!directory.join("measurements").exists());
    assert!(!directory.join(".asb-completions").exists());
    let checkpoint = results.join(".checkpoint");
    assert!(!checkpoint.join("completed/000000.json").exists());
    assert!(!checkpoint.join("replicas/000000/001.json").exists());
    let retained = [
        results.join("plan.json"),
        directory.join("benchmark.toml"),
        checkpoint.join("started/000000/001.json"),
    ]
    .into_iter()
    .map(|path| {
        let bytes = std::fs::read(&path).unwrap();
        (path, bytes)
    })
    .collect::<Vec<_>>();
    let report = root.join("results/sweep-comparison/comparison.json");
    let report_bytes = std::fs::read(&report).unwrap();
    let compared: serde_json::Value = serde_json::from_slice(&report_bytes).unwrap();
    assert_eq!(compared["benchmarks"][0]["result_source"], "run-log");
    assert_eq!(compared["benchmarks"][0]["planning"], log["planning"]);
    assert_eq!(
        compared["benchmarks"][0]["effective_budgets"],
        log["effective_budgets"]
    );
    assert_eq!(
        compared["benchmarks"][0]["benchmark_outcome"],
        log["benchmark_outcome"]
    );
    if case.contains("native") {
        assert_eq!(
            compared["benchmarks"][0]["metrics"]["benchmark_rewards.accuracy@1.accuracy.ratio"],
            0.5
        );
        assert!(
            compared["benchmarks"][0]["metrics"]
                .get("success_rate")
                .is_none()
        );
    } else {
        assert_eq!(
            compared["benchmarks"][0]["metrics"]["success_rate"],
            if case.contains("false") { 0.0 } else { 1.0 }
        );
    }
    assert!(
        compared["benchmarks"][0]["status"]
            .as_str()
            .unwrap()
            .contains("capture limit")
    );
    let html = std::fs::read_to_string(report.with_file_name("report.html")).unwrap();
    assert!(html.contains("capture limit"));
    if case.contains("native") {
        assert!(html.contains("accuracy v1 / accuracy"));
    }
    assert!(!html.contains("fixture-file-grade"));
    crate::compare::generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    assert_eq!(std::fs::read(&report).unwrap(), report_bytes);
    let cli = Command::new(std::env::var_os("ASB_RUNTIME_EXECUTABLE").unwrap())
        .current_dir(root)
        .args(["compare", "sweep.toml"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .output()
        .unwrap();
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(
        cli_report["benchmarks"][0]["benchmark_outcome"],
        log["benchmark_outcome"]
    );
    assert_eq!(cli_report["benchmarks"][0]["result_source"], "run-log");
    assert!(
        cli_report["benchmarks"][0]["status"]
            .as_str()
            .unwrap()
            .contains("capture limit")
    );
    // Relative CLI invocation legitimately renders relative display paths;
    // restoring the same absolute invocation must restore identical report bytes.
    crate::compare::generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
    assert_eq!(std::fs::read(&report).unwrap(), report_bytes);
    assert_eq!(std::fs::read(directory.join("run.log")).unwrap(), log_bytes);
    // A malformed/mismatched latest record must not fall back to an earlier
    // valid grade, nor replace the previously published report generation.
    for (path, value) in [
        ("/agent", serde_json::json!("other-agent")),
        ("/provider", serde_json::json!("other-provider")),
        ("/model", serde_json::json!("other-model")),
        ("/task/task_id", serde_json::json!("other-task")),
        ("/replica_index", serde_json::json!(2)),
    ] {
        let mut invalid = log.clone();
        *invalid.pointer_mut(path).unwrap() = value;
        let bytes = [
            log_bytes.as_slice(),
            b"\n",
            invalid.to_string().as_bytes(),
            b"\n",
        ]
        .concat();
        std::fs::write(directory.join("run.log"), &bytes).unwrap();
        assert!(
            crate::compare::generate(manifest.to_str().unwrap(), &mut Vec::new()).is_err(),
            "{path}"
        );
        assert_eq!(std::fs::read(&report).unwrap(), report_bytes);
        assert_eq!(std::fs::read(directory.join("run.log")).unwrap(), bytes);
    }
    std::fs::write(
        directory.join("run.log"),
        [log_bytes.as_slice(), b"\nnot-json\n"].concat(),
    )
    .unwrap();
    assert!(crate::compare::generate(manifest.to_str().unwrap(), &mut Vec::new()).is_err());
    assert_eq!(std::fs::read(&report).unwrap(), report_bytes);
    std::fs::write(directory.join("run.log"), &log_bytes).unwrap();
    let resume = crate::sweep::Options {
        resume: true,
        ..options
    };
    let error = crate::sweep::run_with_options(manifest.to_str().unwrap(), resume, &mut Vec::new())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("outcome") || error.contains("unsealed") || error.contains("incomplete"),
        "{error}"
    );
    assert_eq!(std::fs::read(directory.join("run.log")).unwrap(), log_bytes);
    assert_eq!(std::fs::read(&report).unwrap(), report_bytes);
    assert!(!directory.join(".asb-completions").exists());
    assert!(!checkpoint.join("completed/000000.json").exists());
    assert!(!checkpoint.join("replicas/000000/001.json").exists());
    for (path, bytes) in retained {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert_eq!(std::fs::read_to_string(counter).unwrap(), "called\n");
    assert_eq!(
        calls.borrow().as_slice(),
        ["evaluate", "finish", "provider-finish"]
    );
}

fn check_full_entry_disposition(
    case: &str,
    root: &Path,
    result: Result<(), Error>,
    calls: &Rc<RefCell<Vec<&'static str>>>,
    output: &[u8],
    completed: usize,
) {
    let expected_kind = if case.contains("run") || case.contains("cleanup") {
        Some("run_error")
    } else if case.contains("client") {
        Some("agent_failure")
    } else if case.contains("grader") {
        Some("grader_error")
    } else if case.contains("false") {
        Some("workload_check_failed")
    } else if case.contains("collector") {
        Some("collector_error")
    } else if case.contains("output") {
        Some("output_error")
    } else if case.contains("provider") {
        Some("provider_cleanup_error")
    } else {
        None
    };
    let error = result.err().map(|error| error.to_string());
    assert_eq!(
        error.is_some(),
        expected_kind.is_some(),
        "{case}: {error:?}"
    );
    assert_eq!(
        calls.borrow().as_slice(),
        ["evaluate", "finish", "provider-finish"]
    );
    let log: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("run.jsonl")).unwrap()).unwrap();
    let measured: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("measurements/run.json")).unwrap())
            .unwrap();
    let expected_status = if expected_kind.is_some() {
        "failed"
    } else {
        "evaluation-pending"
    };
    for record in [&log, &measured] {
        assert_eq!(record["status"], expected_status, "{case}");
        assert_eq!(record["failure_kind"].as_str(), expected_kind, "{case}");
        assert_eq!(
            record["workload_resource_disposition"],
            if case.contains("cleanup") {
                serde_json::Value::Null
            } else {
                serde_json::json!("not-required")
            },
            "resource settlement must remain independent of evaluation and provider cleanup"
        );
        assert_eq!(record["workload_artifacts"], log["workload_artifacts"]);
        assert_eq!(record["process_success"], !case.contains("client"));
        assert_eq!(
            record["benchmark_outcome"]["cleanup_confirmed"],
            !case.contains("cleanup") && !case.contains("provider")
        );
        if case.contains("pending") || case.contains("grader") {
            assert!(record["workload_success"].is_null());
            assert!(record["success_rate"].is_null());
            assert_eq!(
                record["benchmark_outcome"]["evaluation"]["status"],
                if case.contains("pending") {
                    "pending"
                } else {
                    "error"
                }
            );
        } else if case.contains("native") {
            assert!(record["workload_success"].is_null());
            assert!(record["benchmark_outcome"]["evaluation"]["resolution"].is_null());
            assert_eq!(
                record["benchmark_outcome"]["evaluation"]["rewards"]["accuracy"],
                0.5
            );
        } else {
            assert_eq!(record["workload_success"], !case.contains("false"));
            assert_eq!(
                record["success_rate"],
                if case.contains("false") { 0.0 } else { 1.0 }
            );
        }
    }
    if case.contains("output") {
        assert!(
            output.is_empty(),
            "the actual presentation writer must remain broken"
        );
        assert!(error.as_ref().unwrap().contains("presentation closed"));
    } else {
        let summary: serde_json::Value = serde_json::from_slice(output).unwrap();
        assert!(
            summary.get("status").is_none(),
            "do not add a public JSONL status field"
        );
        assert_eq!(summary["failure_kind"], log["failure_kind"]);
        assert_eq!(summary["benchmark_outcome"], log["benchmark_outcome"]);
        assert_eq!(
            summary["workload_resource_disposition"],
            log["workload_resource_disposition"]
        );
        assert_eq!(summary["workload_artifacts"], log["workload_artifacts"]);
    }
    assert_eq!(
        measured["success"],
        case == "disposition-true-output",
        "legacy measurement success remains grade-derived"
    );
    if let Some(error) = &error {
        assert!(
            error.starts_with(log["failure_reason"].as_str().unwrap()),
            "{case}: {error}"
        );
    }
    // Private outcome records preserve pending/error grades for inspection;
    // they are explicitly not completed receipts. The existing checkpoint
    // constructor rejects pending/error or uncertain cleanup before sealing.
    assert!(!root.join(".checkpoint").exists());
    assert_eq!(completed, usize::from(!case.contains("provider")));
    if !case.contains("provider") {
        let records: Vec<_> = std::fs::read_dir(root.join(".asb-completions"))
            .unwrap()
            .collect();
        assert_eq!(records.len(), 1);
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(records[0].as_ref().unwrap().path()).unwrap())
                .unwrap();
        assert_eq!(record["scope"], "task-outcome-not-yet-a-resume-receipt");
        assert_eq!(
            record["summary"]["benchmark_outcome"],
            log["benchmark_outcome"]
        );
        assert_eq!(record["summary"]["failure_kind"], log["failure_kind"]);
        assert_eq!(
            record["workload_resource_disposition"],
            log["workload_resource_disposition"]
        );
        assert_eq!(
            record["summary"]["workload_artifacts"],
            log["workload_artifacts"]
        );
    }
    let report = crate::compare::generate(
        root.join("benchmark.toml").to_str().unwrap(),
        &mut Vec::new(),
    )
    .unwrap();
    let comparison: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report.with_file_name("comparison.json")).unwrap())
            .unwrap();
    let row = &comparison["benchmarks"][0];
    assert_eq!(row["benchmark_outcome"], log["benchmark_outcome"]);
    if expected_kind.is_some() {
        let reason = log["failure_reason"].as_str().unwrap();
        assert!(
            row["status"].as_str().unwrap().contains(reason),
            "{case}: {}",
            row["status"]
        );
        assert!(std::fs::read_to_string(report).unwrap().contains(reason));
    } else {
        assert_eq!(row["status"], "evaluation-pending");
    }
}
#[test]
fn durable_guest_presentation_requires_more_than_a_truncated_stream() {
    for (tree_quiesced, artifact_settled, execution_ok, exit_code, accepted) in [
        (true, true, true, Some(0), true),
        (false, true, true, Some(0), false),
        (true, false, true, Some(0), false),
        (true, true, false, Some(0), false),
        (true, true, true, Some(1), false),
    ] {
        assert_eq!(
            super::durable_guest_presentation_parts(
                true,
                tree_quiesced,
                artifact_settled,
                execution_ok,
                exit_code,
            ),
            accepted,
        );
    }
    assert!(super::durable_guest_presentation_parts(
        false, false, false, false, None
    ));
}
