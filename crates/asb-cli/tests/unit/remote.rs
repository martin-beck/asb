//! Isolated controller fixtures: no Docker, workload SDK, agent or provider.
use super::super::recovery::{AttemptReference, RecoveryContext, RetainedOutcomeContext};
use super::super::session::DatasetIdentity;
use super::*;
use std::fs;
use std::path::Path;

struct Fixture {
    root: PathBuf,
    project: crate::project::ProjectConfig,
    reference: AttemptReference,
}

impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/remote-fixtures")
            .join(TaskIdentity::local("test", None).attempt_id);
        fs::create_dir_all(root.join(".asb/attempts")).unwrap();
        let root = root.canonicalize().unwrap();
        let project = crate::project::ProjectConfig::default();
        let reference = AttemptReference {
            schema_version: 1,
            preparation_id: "ab-cd".into(),
            workload: "fixture".into(),
            task_id: "task-1".into(),
            dataset: DatasetIdentity {
                source: "fixture/dataset".into(),
                revision: "a".repeat(40),
                split: "test".into(),
            },
            artifacts: root.join(".asb/attempts/ab-cd"),
        };
        fs::create_dir_all(&reference.artifacts).unwrap();
        Self {
            root,
            project,
            reference,
        }
    }

    fn tool(&self) -> PathBuf {
        self.root.join(&self.project.tools_dir).join("fixture-sdk")
    }

    #[cfg(unix)]
    fn install_sdk(&self) {
        let bin = self.tool().join("venv/bin");
        fs::create_dir_all(&bin).unwrap();
        let python = crate::tools::host_executable(Path::new("python3"))
            .expect("offline Python fixture interpreter");
        std::os::unix::fs::symlink(python, bin.join("python")).unwrap();
    }

    fn context(&self) -> RecoveryContext<'_> {
        RecoveryContext {
            project_root: &self.root,
            project_config: &self.project,
            reference: &self.reference,
        }
    }

    fn responder(&self, filename: &str, response: &Value, read_request: bool) {
        let request = if read_request {
            "request=json.loads(sys.stdin.readline())\nassert isinstance(request,dict)\n"
        } else {
            ""
        };
        let script = format!(
            "import json,sys\n{request}response=json.loads({:?})\nprint(json.dumps(response),flush=True)\n",
            serde_json::to_string(response).unwrap()
        );
        fs::write(self.tool().join(filename), script).unwrap();
    }

    fn grade(&self) -> Value {
        json!({"schema_version":1,"scope":"retained-official-grade-not-execution-recovery",
            "task":{"workload":self.reference.workload,"task_id":self.reference.task_id,"dataset":self.reference.dataset,"attempt_id":format!("asb-{}","a".repeat(32))},
            "workload_baseline":{"kind":"upstream-task-definition","base_commit":"b".repeat(40)},
            "workload_evaluation":"upstream-harness","workload_success":false,"recorded_cleanup":"stopped-preserved",
            "evidence":{"task.json":{"sha256":"d".repeat(64),"bytes":100}},
            "process_success":null,"exit_code":null,"elapsed_ms":null,"token_usage":null,"steps":null})
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn sdk_presence_and_inspection_runtime_cleanup_are_project_local() {
    let fixture = Fixture::new();
    let error = require_sdk(&fixture.tool(), "fixture")
        .unwrap_err()
        .to_string();
    assert!(error.contains("asb workload install fixture"));
    let runtime = InspectionRuntime::new(&fixture.root).unwrap();
    let path = runtime.0.clone();
    fs::write(path.join("support.py"), "fixture").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    drop(runtime);
    assert!(!path.exists());
    assert!(fixture.reference.artifacts.exists());
    assert!(InspectionRuntime::new(&fixture.root.join("missing-parent")).is_err());
}

#[cfg(unix)]
#[test]
fn retained_verification_accepts_only_consistent_safe_bounded_responses() {
    let fixture = Fixture::new();
    fixture.install_sdk();
    let summary = json!({"workload_success":false});
    let context = RetainedOutcomeContext {
        recovery: fixture.context(),
        summary: &summary,
    };
    fixture.responder(
        "asb_retained.py",
        &json!({"ok":true,"scope":"retained-artifact-consistency-not-historical-attestation"}),
        true,
    );
    verify_retained("fixture-sdk", "fixture_module", &context).unwrap();
    for response in [
        json!({"ok":false,"reason":"changed task artifact"}),
        json!({"ok":true,"scope":"wrong","reason":"changed task artifact"}),
        json!({"ok":false,"reason":"secret\nunsafe"}),
        json!({"ok":false,"reason":"x".repeat(513)}),
        json!({"ok":false}),
    ] {
        fixture.responder("asb_retained.py", &response, true);
        let error = verify_retained("fixture-sdk", "fixture_module", &context)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot reconcile retained outcome"));
        if response["reason"] == "changed task artifact" {
            assert!(error.contains("changed task artifact"));
        } else {
            assert!(error.contains("do not establish a consistent completed outcome"));
        }
        assert!(!error.contains("secret"));
    }
}

#[cfg(unix)]
#[test]
fn retained_grade_protocol_preserves_valid_grades_and_sanitizes_failure_reasons() {
    let fixture = Fixture::new();
    fixture.install_sdk();
    let report = fixture.grade();
    fixture.responder("asb_retained.py", &report, true);
    assert_eq!(
        inspect_grade(
            "fixture-sdk",
            "fixture_module",
            &fixture.context(),
            &fixture.tool()
        )
        .unwrap(),
        report
    );
    assert_eq!(
        grade_retained(
            "fixture-sdk",
            "fixture_module",
            &fixture.context(),
            &fixture.tool(),
            &fixture.root.join("catalog.json")
        )
        .unwrap(),
        report
    );
    for (response, expected) in [
        (
            json!({"ok":false,"reason":"missing official report","error_class":"FixtureError"}),
            "missing official report",
        ),
        (
            json!({"ok":false,"reason":"unsafe\nreason","error_class":"Missing_Report"}),
            "Missing_Report",
        ),
        (
            json!({"ok":false,"reason":"x".repeat(513),"error_class":"bad class!"}),
            "unknown_error",
        ),
        (json!({"ok":false,"error_class":""}), "unknown_error"),
    ] {
        fixture.responder("asb_retained.py", &response, true);
        let error = inspect_grade(
            "fixture-sdk",
            "fixture_module",
            &fixture.context(),
            &fixture.tool(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains(expected), "{error}");
        assert!(error.contains("cannot inspect retained grade"));
        assert!(error.contains("no agent was run"));
        assert!(!error.contains("unsafe\n"));
        let error = grade_retained(
            "fixture-sdk",
            "fixture_module",
            &fixture.context(),
            &fixture.tool(),
            &fixture.root.join("catalog.json"),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cannot complete retained grade"));
    }
}

#[cfg(unix)]
#[test]
fn recovery_validates_resource_ownership_identity_and_bounded_reports() {
    let fixture = Fixture::new();
    fixture.install_sdk();
    assert!(
        recover("fixture-sdk", &fixture.context())
            .unwrap_err()
            .to_string()
            .contains("helper is missing")
    );
    let attempt = format!("asb-{}", "a".repeat(32));
    let report = json!({"schema_version":1,"attempt":attempt,"scope":"observed-resource-quiescence-not-a-retry-receipt","resources":[
        {"kind":"containers","status":"stopped-preserved","name":format!("{attempt}-task")},
        {"kind":"images","status":"preserved","name":format!("{attempt}-image"),"image_id":format!("sha256:{}","b".repeat(64))},
        {"kind":"networks","status":"absent","name":format!("{attempt}-network")},
        {"kind":"volumes","status":"preserved","name":format!("{attempt}-volume")}
    ]});
    fixture.responder("asb_resources.py", &report, false);
    assert_eq!(recover("fixture-sdk", &fixture.context()).unwrap(), report);
    let mut invalids = Vec::new();
    for (field, value) in [
        ("schema_version", json!(2)),
        ("attempt", json!("asb-bad")),
        ("scope", json!("retry")),
        ("resources", Value::Null),
        ("resources", json!(vec![report["resources"][0].clone(); 65])),
    ] {
        let mut invalid = report.clone();
        invalid[field] = value;
        invalids.push(invalid);
    }
    for (index, field, value) in [
        (0, "kind", json!("processes")),
        (0, "status", json!("running")),
        (0, "name", json!("unrelated")),
        (0, "name", json!("x".repeat(256))),
        (1, "image_id", json!("sha256:bad")),
        (1, "image_id", Value::Null),
    ] {
        let mut invalid = report.clone();
        invalid["resources"][index][field] = value;
        invalids.push(invalid);
    }
    for invalid in invalids {
        fixture.responder("asb_resources.py", &invalid, false);
        assert!(
            recover("fixture-sdk", &fixture.context())
                .unwrap_err()
                .to_string()
                .contains("invalid bounded report")
        );
    }
    for (response, reason) in [
        (
            json!({"ok":false,"error_class":"RecoveryError","reason":"owned container did not stop"}),
            "owned container did not stop",
        ),
        (
            json!({"ok":false,"error_class":"UnknownError","reason":"must not echo"}),
            "Docker resource inspection or access failed",
        ),
        (
            json!({"ok":false,"error_class":"RecoveryError","reason":"secret\nresponse"}),
            "Docker resource inspection or access failed",
        ),
    ] {
        fixture.responder("asb_resources.py", &response, false);
        let error = recover("fixture-sdk", &fixture.context())
            .unwrap_err()
            .to_string();
        assert!(error.contains(reason), "{error}");
        assert!(!error.contains("secret"));
    }
}

#[cfg(unix)]
#[test]
fn launch_validates_preparation_before_exposing_a_session_and_preserves_on_failure() {
    let fixture = Fixture::new();
    fixture.install_sdk();
    let workspace = fixture.root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let context = PreparationContext {
        verify_initial_material: false,
        budgets: Default::default(),
        on_allocation: None,
        workload: "swe-bench-lite",
        trial: Some("task-1"),
        trial_set: None,
        workload_directory: &fixture.root.join("workloads"),
        workspace: &workspace,
        project_root: &fixture.root,
        project_config: &fixture.project,
    };
    let ready = json!({"event":"ready","invocation":{
        "identity":{"workload":"swe-bench-lite","task_id":"task-1","attempt_id":format!("asb-{}","a".repeat(32)),"dataset":fixture.reference.dataset},
        "prompt":"Solve the original fixture task.","workspace":"/task",
        "baseline":{"kind":"upstream-task-definition","base_commit":"b".repeat(40)},"agent_timeout_seconds":1
    }});
    let install_controller = |ready: &Value| {
        let source = format!(
            "import json,sys\nready=json.loads({:?})\nprint(json.dumps(ready),flush=True)\nfor line in sys.stdin:\n request=json.loads(line)\n if request['op']=='evaluate':\n  print(json.dumps(dict(event='evaluated',result=dict(workload='swe-bench-lite',task_id='task-1',dataset=ready['invocation']['identity']['dataset'],outcome='resolved',resolved=True))),flush=True)\n elif request['op']=='close':\n  print(json.dumps(dict(event='closed',cleanup='removed')),flush=True)\n  sys.exit(0)\nprint(json.dumps(dict(event='closed',cleanup='stopped-preserved')),flush=True)\n",
            serde_json::to_string(ready).unwrap()
        );
        fs::write(fixture.tool().join("controller.py"), source).unwrap();
    };
    install_controller(&ready);
    let mut session = launch(
        "fixture-sdk",
        "controller.py",
        Some("prepare"),
        "swe-bench-lite",
        fixture.reference.dataset.clone(),
        &context,
    )
    .unwrap();
    assert_eq!(
        session.invocation().input.instructions,
        "Solve the original fixture task."
    );
    assert_eq!(session.invocation().workspace, workspace);
    assert_eq!(session.evaluation_kind().as_str(), "upstream-harness");
    assert_eq!(session.baseline_passed(), None);
    assert!(session.baseline_evidence().supports("upstream-harness"));
    assert_eq!(session.checkpoint_evidence().len(), 1);
    assert!(matches!(session.evaluate("").unwrap(), Evaluation::Passed));
    session.finish().unwrap();
    // Failure before guest bootstrap must cancel/preserve and close the
    // preparation lifecycle, without installing or starting any client.
    let config: crate::benchmark::BenchmarkConfig = serde_json::from_value(json!({
        "agent":"nonexistent-fixture-agent", "provider":"fixture-provider", "model":"fixture-model", "workload":"swe-bench-lite"
    })).unwrap();
    let connection = crate::endpoint::Connection {
        interface: crate::endpoint::Interface::OpenAiChat,
        base_url: "http://127.0.0.1:1/v1".into(),
        credential: None,
        authentication: crate::endpoint::Authentication::None,
    };
    let measurements_dir = fixture.root.join("measurements");
    let mut measurements = crate::measurements::Session::start_in_execution_namespace(
        &measurements_dir,
        &["agent-memory".into()],
    )
    .unwrap();
    measurements
        .event(crate::collectors::LifecycleEvent::RunStarted)
        .unwrap();
    let mut session = launch(
        "fixture-sdk",
        "controller.py",
        None,
        "swe-bench-lite",
        fixture.reference.dataset.clone(),
        &context,
    )
    .unwrap();
    let mut execution = ExecutionContext {
        launch: None,
        config: &config,
        connection: Some(&connection),
        project_root: &fixture.root,
        project_config: &fixture.project,
        replay: true,
        tape_cassette: None,
        tool_replay: false,
        tool_tape_cassette: None,
        measurements: Some(&mut measurements),
        show_output: false,
        progress: None,
    };
    let error = session
        .execute(&mut execution, &mut Vec::new())
        .unwrap()
        .err()
        .unwrap();
    assert!(
        error.contains("cannot prepare selected agent installation artifacts"),
        "{error}"
    );
    assert!(!error.contains("cleanup failed"), "{error}");
    drop(session);
    drop(measurements);
    let journal: Value =
        serde_json::from_slice(&fs::read(measurements_dir.join("lifecycle.json")).unwrap())
            .unwrap();
    let events = journal.as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|entry| entry["event"]["kind"] == "phase_begin"
                && entry["event"]["phase"] == "agent_preparation")
    );
    assert!(
        events
            .iter()
            .any(|entry| entry["event"]["kind"] == "phase_failed"
                && entry["event"]["phase"] == "agent_preparation")
    );
    let mut invalids = Vec::new();
    let mut not_ready = ready.clone();
    not_ready["event"] = json!("started");
    invalids.push((not_ready, "did not become ready"));
    for (path, value, reason) in [
        (vec!["identity"], Value::Null, "invalid identity"),
        (
            vec!["identity", "task_id"],
            json!("wrong-task"),
            "does not match",
        ),
        (vec!["prompt"], json!(""), "invalid public instructions"),
        (
            vec!["workspace"],
            json!("relative/task"),
            "invalid environment workspace",
        ),
        (
            vec!["workspace"],
            json!("/task/../outside"),
            "invalid environment workspace",
        ),
        (
            vec!["baseline"],
            Value::Null,
            "baseline evidence is invalid",
        ),
        (
            vec!["baseline"],
            json!({"kind":"unavailable"}),
            "does not establish baseline identity",
        ),
        (
            vec!["agent_timeout_seconds"],
            json!(0),
            "invalid agent time budget",
        ),
        (
            vec!["agent_timeout_seconds"],
            json!(86401),
            "invalid agent time budget",
        ),
    ] {
        let mut invalid = ready.clone();
        let mut target = &mut invalid["invocation"];
        for key in path {
            target = &mut target[key];
        }
        *target = value;
        invalids.push((invalid, reason));
    }
    for (invalid, reason) in invalids {
        install_controller(&invalid);
        let error = launch(
            "fixture-sdk",
            "controller.py",
            None,
            "swe-bench-lite",
            fixture.reference.dataset.clone(),
            &context,
        )
        .err()
        .unwrap();
        assert!(error.contains(reason), "{error}");
        assert!(!error.contains("cancellation cleanup failed"), "{error}");
    }
    let no_trial = PreparationContext {
        trial: None,
        ..context
    };
    assert!(
        launch(
            "fixture-sdk",
            "controller.py",
            None,
            "swe-bench-lite",
            fixture.reference.dataset.clone(),
            &no_trial
        )
        .err()
        .unwrap()
        .contains("select an official instance ID")
    );
}
