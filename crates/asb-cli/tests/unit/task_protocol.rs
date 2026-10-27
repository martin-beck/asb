use super::*;
use serde_json::json;

fn dataset() -> DatasetIdentity {
    DatasetIdentity {
        source: "fixture/dataset".into(),
        revision: "a".repeat(40),
        split: "test".into(),
    }
}

fn ready() -> Value {
    json!({"event":"ready","invocation":{
        "identity":{"workload":"fixture","task_id":"task","attempt_id":"attempt","dataset":dataset()},
        "prompt":"public issue","workspace":"/task",
        "baseline":{"kind":"upstream-task-definition","base_commit":"b".repeat(40)}
    }})
}

#[test]
fn ready_constructor_checks_every_existing_identity_instruction_baseline_and_budget_obligation() {
    let valid = ready();
    let decoded = ReadyTask::decode(&valid, "fixture", "task", &dataset()).unwrap();
    assert_eq!(decoded.identity.task_id, "task");
    assert_eq!(decoded.prompt, "public issue");
    assert_eq!(decoded.guest_workspace, "/task");
    assert_eq!(decoded.agent_timeout, DEFAULT_AGENT_TIMEOUT_SECONDS);
    assert!(decoded.baseline.supports("upstream-harness"));
    for (path, value) in [
        ("/event", json!("completed")),
        ("/invocation/identity", Value::Null),
        ("/invocation/identity/workload", json!("other")),
        ("/invocation/identity/task_id", json!("other")),
        ("/invocation/identity/dataset/revision", json!("other")),
        ("/invocation/prompt", json!("")),
        ("/invocation/prompt", json!("x".repeat(64 * 1024 + 1))),
        ("/invocation/workspace", json!("relative")),
        ("/invocation/workspace", json!("/task/../other")),
        ("/invocation/baseline", Value::Null),
        ("/invocation/baseline", json!({"kind":"unavailable"})),
        ("/invocation/agent_timeout_seconds", json!(0)),
        ("/invocation/agent_timeout_seconds", json!(86401)),
    ] {
        let mut bad = valid.clone();
        if path.ends_with("agent_timeout_seconds") {
            bad["invocation"]["agent_timeout_seconds"] = value;
        } else {
            *bad.pointer_mut(path).unwrap() = value;
        }
        assert!(
            ReadyTask::decode(&bad, "fixture", "task", &dataset()).is_err(),
            "{path}"
        );
    }
    // Preserve existing absent/non-numeric fallback and unknown extra fields.
    for budget in [Value::Null, json!("historical"), json!(0.001), json!(86400)] {
        let mut compatible = valid.clone();
        compatible["invocation"]["agent_timeout_seconds"] = budget;
        compatible["future"] = json!(true);
        assert!(ReadyTask::decode(&compatible, "fixture", "task", &dataset()).is_ok());
    }
}

fn completion() -> Value {
    json!({"event":"completed","result":{"exit_code":7,"stdout":"AP+A","stderr":"",
        "elapsed_seconds":0.125,"metadata":{"schema_version":1,"exit_code":7,"final_response":null,
        "token_usage":{},"steps":null,"execution_failure":null,"tree_quiesced":true}}})
}

#[test]
fn optional_guest_termination_is_independent_of_carrier_and_cleanup_permission() {
    for exact in [
        json!({"kind":"code","code":143}),
        json!({"kind":"signal","signal":15}),
    ] {
        let mut value = completion();
        value["result"]["exit_code"] = json!(1);
        value["result"]["metadata"]["process_termination"] = exact.clone();
        let result = CompletedExecution::decode(&value, ProcessLifetime::Quiesce)
            .unwrap()
            .into_execution()
            .unwrap();
        assert_eq!(result.output.status.code(), Some(1));
        assert!(result.metadata.tree_quiesced);
        assert_eq!(
            serde_json::to_value(result.metadata.observation.process_termination).unwrap(),
            exact
        );
        value["result"]["process_termination"] = json!({"kind":"code","code":7});
        let result = CompletedExecution::decode(&value, ProcessLifetime::Quiesce)
            .unwrap()
            .into_execution()
            .unwrap();
        assert_eq!(
            result.metadata.observation,
            crate::process_termination::Observation::malformed()
        );
        assert_eq!(result.output.status.code(), Some(1));
        assert!(result.metadata.tree_quiesced);
    }
    let mut value = completion();
    value["result"]["metadata"]["process_termination"] = json!({"kind":"signal","signal":0});
    let decoded = CompletedExecution::decode(&value, ProcessLifetime::Quiesce).unwrap();
    assert_eq!(
        decoded.metadata().observation,
        crate::process_termination::Observation::malformed()
    );
    value["result"]["metadata"]["tree_quiesced"] = json!(false);
    assert!(
        CompletedExecution::decode(&value, ProcessLifetime::Quiesce).is_err(),
        "optional diagnostics cannot rescue missing ownership proof"
    );
}

#[test]
fn completed_view_preserves_observations_defaults_and_quiescence_proof() {
    let value = completion();
    let result = CompletedExecution::decode(&value, ProcessLifetime::Quiesce)
        .unwrap()
        .into_execution()
        .unwrap();
    assert_eq!(result.output.stdout, [0, 255, 128]);
    assert_eq!(result.output.status.code(), Some(7));
    assert_eq!(result.elapsed_ms, Some(125));
    for (path, invalid) in [
        ("/event", json!("unknown")),
        ("/result/metadata", Value::Null),
        ("/result/metadata/tree_quiesced", json!(false)),
        ("/result/stdout", json!("YR==")),
        ("/result/stderr", Value::Null),
    ] {
        let mut bad = value.clone();
        *bad.pointer_mut(path).unwrap() = invalid;
        assert!(
            CompletedExecution::decode(&bad, ProcessLifetime::Quiesce).is_err(),
            "{path}"
        );
    }
    for (code, expected) in [(Value::Null, 1), (json!(-1), 0), (json!(256), 255)] {
        let mut old = value.clone();
        old["result"]["exit_code"] = code;
        old["result"]["elapsed_seconds"] = json!(-1);
        let result = CompletedExecution::decode(&old, ProcessLifetime::Quiesce)
            .unwrap()
            .into_execution()
            .unwrap();
        assert_eq!(result.output.status.code(), Some(expected));
        assert_eq!(result.elapsed_ms, None);
    }
}

#[test]
fn delegated_completion_requires_declared_lifetime_retention_and_every_owner_binding() {
    let mut valid = completion();
    valid["result"]["metadata"]["tree_quiesced"] = json!(false);
    valid["result"]["workload_processes_retained"] = json!(true);
    valid["result"]["metadata"]["workload_owner"] = json!({
        "controller_namespace":2,"guest_namespace":3,"token":"a".repeat(32)});
    assert!(CompletedExecution::decode(&valid, ProcessLifetime::ThroughEvaluation).is_ok());
    assert!(CompletedExecution::decode(&valid, ProcessLifetime::Quiesce).is_err());
    for (path, invalid) in [
        ("/result/workload_processes_retained", json!(false)),
        ("/result/metadata/workload_owner", Value::Null),
        (
            "/result/metadata/workload_owner/controller_namespace",
            json!(1),
        ),
        ("/result/metadata/workload_owner/guest_namespace", json!(1)),
        ("/result/metadata/workload_owner/guest_namespace", json!(2)),
        (
            "/result/metadata/workload_owner/token",
            json!("a".repeat(31)),
        ),
        (
            "/result/metadata/workload_owner/token",
            json!("z".repeat(32)),
        ),
    ] {
        let mut bad = valid.clone();
        *bad.pointer_mut(path).unwrap() = invalid;
        assert!(
            CompletedExecution::decode(&bad, ProcessLifetime::ThroughEvaluation).is_err(),
            "{path}"
        );
    }
}

#[test]
fn guest_sidecars_cross_the_control_boundary_as_receipts_not_json_bytes() {
    use sha2::{Digest, Sha256};
    let root = std::env::temp_dir().join(format!(
        "asb-guest-artifact-{}",
        crate::workloads::TaskIdentity::local("receipt", None).attempt_id
    ));
    std::fs::create_dir_all(&root).unwrap();
    let bytes = b"sealed relay bundle";
    std::fs::write(root.join("artifact-0"), bytes).unwrap();
    let mut value = completion();
    value["result"]["metadata"]["exported_artifacts"] = json!([{
        "kind":"mcp-relay-filesystem",
        "filename":"artifact-0",
        "bytes":bytes.len(),
        "sha256":format!("{:x}", Sha256::digest(bytes))
    }]);
    let result =
        CompletedExecution::decode_with_artifacts(&value, ProcessLifetime::Quiesce, Some(&root))
            .unwrap()
            .into_execution()
            .unwrap();
    assert_eq!(result.metadata.sealed_artifacts.len(), 1);
    assert_eq!(result.metadata.sealed_artifacts[0].bytes, bytes);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn guest_sidecar_receipts_fail_closed_for_missing_or_changed_files() {
    let root = std::env::temp_dir().join(format!(
        "asb-guest-artifact-invalid-{}",
        crate::workloads::TaskIdentity::local("receipt", None).attempt_id
    ));
    std::fs::create_dir_all(&root).unwrap();
    let mut value = completion();
    value["result"]["metadata"]["exported_artifacts"] = json!([{
        "kind":"mcp-relay-filesystem",
        "filename":"artifact-0",
        "bytes":1,
        "sha256":"a".repeat(64)
    }]);
    assert!(
        CompletedExecution::decode_with_artifacts(&value, ProcessLifetime::Quiesce, Some(&root),)
            .is_err()
    );
    std::fs::write(root.join("artifact-0"), b"x").unwrap();
    assert!(
        CompletedExecution::decode_with_artifacts(&value, ProcessLifetime::Quiesce, Some(&root),)
            .is_err()
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn completed_grade_is_not_process_success_or_an_unknown_or_inconsistent_resolution() {
    let expected: TaskIdentity =
        serde_json::from_value(ready()["invocation"]["identity"].clone()).unwrap();
    for (outcome, resolved) in [
        ("resolved", true),
        ("unresolved", false),
        ("empty_submission", false),
    ] {
        let event = json!({"event":"evaluated","result":{"workload":"fixture","task_id":"task",
            "dataset":dataset(),"outcome":outcome,"resolved":resolved}});
        let grade = CompletedGrade::decode(&event, &expected).unwrap();
        assert_eq!(matches!(grade.evaluation(), Evaluation::Passed), resolved);
        for (path, invalid) in [
            ("/event", json!("completed")),
            ("/result/workload", json!("other")),
            ("/result/task_id", json!("other")),
            ("/result/dataset/revision", json!("other")),
            ("/result/outcome", json!("pending")),
            ("/result/outcome", json!("evaluation_error")),
            ("/result/outcome", json!("future-kind")),
            ("/result/resolved", json!(!resolved)),
            ("/result/resolved", Value::Null),
            ("/result/resolved", json!("false")),
        ] {
            let mut bad = event.clone();
            *bad.pointer_mut(path).unwrap() = invalid;
            assert!(
                CompletedGrade::decode(&bad, &expected).is_err(),
                "{outcome} {path}"
            );
        }
        let mut missing = event.clone();
        missing["result"]
            .as_object_mut()
            .unwrap()
            .remove("resolved");
        assert!(CompletedGrade::decode(&missing, &expected).is_err());
    }
}

#[test]
fn typed_cleanup_acknowledgements_preserve_positive_settlement_and_reject_unknowns() {
    for cleanup in [
        "removed",
        "not_created",
        "stopped-preserved",
        "pending",
        "error",
        "ownership_mismatch",
        "cleanup_failed",
        "future",
    ] {
        let event = json!({"event":"closed","cleanup":cleanup});
        assert_eq!(
            NormalCloseAck::decode(&event).is_ok(),
            matches!(cleanup, "removed" | "not_created" | "stopped-preserved")
        );
        assert_eq!(
            CancellationAck::decode(&event).is_ok(),
            matches!(cleanup, "removed" | "not_created" | "stopped-preserved")
        );
    }
    for event in [
        json!({}),
        json!({"event":"closed"}),
        json!({"event":"closed","cleanup":null}),
        json!({"event":"completed","cleanup":"removed"}),
        json!({"event":"future","cleanup":"removed"}),
    ] {
        assert!(NormalCloseAck::decode(&event).is_err());
        assert!(CancellationAck::decode(&event).is_err());
    }
}

#[test]
fn rust_request_and_python_producer_event_spellings_match_the_original_wire() {
    let resume = json!("owned-exec");
    for (request, expected) in [
        (Request::Close, json!({"op":"close"})),
        (
            Request::InitialMaterial {
                binary: Path::new("fixture-asb"),
            },
            json!({"op":"initial-material","binary":"fixture-asb"}),
        ),
        (
            Request::Evaluate { timeout: 60 },
            json!({"op":"evaluate","timeout":60}),
        ),
        (
            Request::Resume { exec_id: &resume },
            json!({"op":"resume","exec_id":"owned-exec"}),
        ),
    ] {
        assert_eq!(request.value().unwrap(), expected);
    }
    let bootstrap = crate::install::GuestBootstrap::fixture_empty();
    let agent = Request::Agent {
        payload: Box::new(AgentRequest {
            binary: Path::new("fixture-asb"),
            project_root: Path::new("fixture-project"),
            preparation_seconds: Some(1800.0),
            timeout: 1.0,
            tools_root: Path::new("fixture-tools"),
            bootstrap: &bootstrap,
            measure_target: false,
            project_configuration: "fixture".into(),
            environment: serde_json::Map::new(),
            network: None,
            artifact_output: Path::new("fixture-artifacts"),
            private_inputs: Vec::new(),
            request: json!({"replay":true}),
        }),
    }
    .value()
    .unwrap();
    assert_eq!(
        agent,
        json!({"op":"agent","binary":"fixture-asb","project_root":"fixture-project","preparation_seconds":1800.0,"timeout":1.0,
        "tools_root":"fixture-tools","bootstrap":{"runtime_tools":[],"assets":[],"public_package_cache":false,"npm_package_tree":null},
        "measure_target":false,"project_configuration":"fixture","environment":{},"network":null,"artifact_output":"fixture-artifacts","private_inputs":[],"request":{"replay":true}})
    );
    let output = std::process::Command::new("python3")
        .args(["-I","-S","-B","-c", "import json,sys;sys.path.insert(0,sys.argv[1]);from asb_task_server import Event,Operation;print(json.dumps(dict(events=[e.value for e in Event],operations=[o.value for o in Operation])))"])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/benchmarks"))
        .output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let python: Value = serde_json::from_slice(&output.stdout).unwrap();
    let events = [
        "ready",
        "paused",
        "output",
        "execution_ended",
        "completed",
        "submission",
        "evaluated",
        "error",
        "closed",
        "initial-material",
        "preparation_failed",
    ];
    assert_eq!(python["events"], json!(events));
    assert_eq!(
        python["operations"],
        json!([
            "execute",
            "agent",
            "resume",
            "capture",
            "evaluate",
            "close",
            "cancel",
            "initial-material"
        ])
    );
    for name in events {
        let value = json!({"event":name});
        let kind = EventKind::of(&value).unwrap();
        assert_eq!(serde_json::to_value(kind).unwrap(), json!(name));
    }
    for malformed in [
        json!({}),
        json!({"event":null}),
        json!({"event":0}),
        json!({"event":"future"}),
    ] {
        assert_eq!(EventKind::of(&malformed), None);
    }
}
