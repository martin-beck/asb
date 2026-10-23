use super::*;
use crate::workloads::summary::update_summary as update_summary_value;
use serde::ser::{Error as _, SerializeMap};

#[cfg(target_os = "linux")]
#[test]
fn perf_interrupt_result_preserves_signal_grade_and_raw_on_invalid_or_unrequested_capture() {
    use std::os::unix::process::ExitStatusExt;
    for case in ["valid", "bad-data", "incomplete", "unsolicited"] {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("perf-result", None).attempt_id);
        let mut session = Session::start_with_collectors(&root, &[], None).unwrap();
        stopped_fixture(&mut session, "perf-stat");
        let mut stopped = session.stopped_collectors.pop().unwrap();
        stopped.collector.adapter = collectors::find("perf").unwrap();
        fs::write(
            root.join("perf-stat.stderr"),
            if case == "bad-data" {
                "partial counter"
            } else {
                "1.0,42,,cycles,100,100.0,,\n"
            },
        )
        .unwrap();
        let mut completion =
            ProducerCompletion::test_completed(std::process::ExitStatus::from_raw(libc::SIGINT));
        completion.interruption_requested = case != "unsolicited";
        completion.interruption_acknowledged = case != "unsolicited";
        completion.guardian_cleanup_confirmed = Some(true);
        completion.capture_complete = case != "incomplete";
        stopped.collector.producer = Producer::observed(completion);
        session.collectors.push(stopped.collector);
        stopped_fixture(&mut session, "sibling");
        let _ = session.finish(fixture_measurement());
        let result: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("perf-stat.json")).unwrap()).unwrap();
        assert_eq!(result["exit_code"], serde_json::Value::Null);
        assert_eq!(result["exit_signal"], libc::SIGINT);
        assert_eq!(
            result["status"],
            if case == "valid" {
                "completed"
            } else {
                "failed"
            }
        );
        assert_eq!(root.join("perf-stat.stdout").exists(), case != "valid");
        assert_eq!(root.join("perf-stat.stderr").exists(), case != "valid");
        assert!(root.join("sibling.json").is_file());
        let run: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("run.json")).unwrap()).unwrap();
        assert_eq!(run["workload_success"], false);
        assert!(root.join("summary.json").is_file());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn incomplete_snapshot_retains_raw_prefix_without_parsing_and_finishes_sibling() {
    let root = std::env::temp_dir().join(format!(
        "asb-incomplete-snapshot-{}",
        crate::tools::runtime::unique_id()
    ));
    let mut session = Session::start_with_collectors(&root, &[], None).unwrap();
    stopped_fixture(&mut session, "admission");
    stopped_fixture(&mut session, "sibling");
    session.incomplete_collectors.insert(
        "admission".into(),
        "producer write failed after a valid prefix".into(),
    );
    let _ = session.finish(fixture_measurement());
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("admission.json")).unwrap()).unwrap();
    assert_eq!(record["status"], "failed");
    assert_eq!(
        record["stdout"], "",
        "incomplete capture must not be read as stopped evidence"
    );
    assert_eq!(record["metrics"], serde_json::json!({}));
    assert!(
        record["stderr"]
            .as_str()
            .unwrap()
            .contains("producer write failed")
    );
    assert_eq!(fs::read(root.join("admission.stdout")).unwrap(), b"A=17\n");
    assert!(root.join("admission.stderr").is_file());
    assert!(!root.join("sibling.stdout").exists());
    assert!(root.join("sibling.json").is_file());
    assert!(root.join("run.json").is_file());
    assert!(root.join("summary.json").is_file());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn later_collector_admission_failure_retains_sources_and_finalizes_admitted_sibling() {
    let root = std::env::temp_dir().join(format!(
        "asb-capture-startup-{}",
        crate::tools::runtime::unique_id()
    ));
    let mut session = Session::start_with_collectors(&root, &[], None).unwrap();
    session
        .event(collectors::LifecycleEvent::RunStarted)
        .unwrap();
    stopped_fixture(&mut session, "admission");
    // Both new captures are established, then target validation fails before
    // any sampler or placeholder child. Existing admitted evidence survives.
    let error = session
        .start_tree_snapshot(
            "agent-memory",
            true,
            Path::new("/bin/false"),
            0,
            Box::new(|_| panic!("invalid target must not admit a sampler")),
        )
        .unwrap_err();
    assert!(error.contains("target has exited"), "{error}");
    assert!(session.collectors.is_empty());
    assert_eq!(fs::read(root.join("admission.stdout")).unwrap(), b"A=17\n");
    session.finish(fixture_measurement()).unwrap();
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("admission.json")).unwrap()).unwrap();
    assert_eq!(record["metrics"]["value"], 17.0);
    assert!(!root.join("admission.stdout").exists());
    for name in ["stdout", "stderr"] {
        assert_eq!(
            fs::read(root.join(format!("agent-memory.{name}"))).unwrap(),
            b""
        );
    }
    for file in [
        "run.json",
        "lifecycle.json",
        "summary.json",
        "components.json",
    ] {
        assert!(root.join(file).is_file());
    }
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn admitted_capture_not_foreign_path_supplies_diagnostics_and_full_metrics() {
    let root = std::env::temp_dir().join(format!(
        "asb-admitted-capture-{}",
        crate::tools::runtime::unique_id()
    ));
    let mut session = Session::start_with_collectors(&root, &[], None).unwrap();
    stopped_fixture(&mut session, "admission");
    stopped_fixture(&mut session, "sibling");
    fs::rename(root.join("admission.stdout"), root.join("admission.saved")).unwrap();
    fs::write(root.join("admission.stdout"), b"A=99\n").unwrap();
    let outcome = session.finish(fixture_measurement());
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("admission.json")).unwrap()).unwrap();
    assert_eq!(
        record["metrics"]["value"], 17.0,
        "adapter parsed a replacement instead of its admitted source"
    );
    assert_eq!(record["stdout"], "A=17\n");
    assert!(outcome.unwrap_err().contains("raw path changed"));
    assert_eq!(fs::read(root.join("admission.stdout")).unwrap(), b"A=99\n");
    assert_eq!(fs::read(root.join("admission.saved")).unwrap(), b"A=17\n");
    assert!(root.join("sibling.json").is_file());
    assert!(root.join("run.json").is_file());
    assert!(root.join("summary.json").is_file());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn capture_changed_between_identity_check_and_move_is_restored_not_deleted() {
    let root = std::env::temp_dir().join(format!(
        "asb-raw-race-{}",
        crate::tools::runtime::unique_id()
    ));
    let mut session = Session::start_with_collectors(&root, &[], None).unwrap();
    stopped_fixture(&mut session, "racing");
    let scope = crate::evidence::tests::Scope::replace_on_capture_move(root.join("racing.stdout"));
    let error = session.finish(fixture_measurement()).unwrap_err();
    assert_eq!(scope.hits(), 1);
    assert!(
        error.contains("foreign entry restored and retained"),
        "{error}"
    );
    assert_eq!(
        fs::read(root.join("racing.stdout")).unwrap(),
        b"racing foreign capture"
    );
    assert_eq!(
        fs::read(root.join("racing.original")).unwrap(),
        b"complete raw stdout"
    );
    let stage = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".asb-evidence-stage-")
        })
        .unwrap();
    assert_eq!(
        fs::read(stage.join("content")).unwrap(),
        b"racing foreign capture"
    );
    assert!(error.contains(stage.to_str().unwrap()));
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn raw_cleanup_refuses_foreign_replacement_and_discloses_owned_removal_failure() {
    for foreign in [true, false] {
        let root = std::env::temp_dir().join(format!(
            "asb-raw-cleanup-{}",
            crate::tools::runtime::unique_id()
        ));
        let mut session = Session::start_with_collectors(&root, &[], None).unwrap();
        session
            .event(collectors::LifecycleEvent::RunStarted)
            .unwrap();
        let name = if foreign { "foreign" } else { "owned" };
        stopped_fixture(&mut session, name);
        let _scope = (!foreign).then(|| {
            crate::evidence::tests::Scope::new(
                root.join("owned.stdout"),
                Some(crate::evidence::FaultPoint::Cleanup),
                vec![],
            )
        });
        let error = session.finish(fixture_measurement()).unwrap_err();
        assert!(root.join(format!("{name}.json")).is_file());
        if foreign {
            assert!(error.contains("raw path changed"), "{error}");
            assert_eq!(
                fs::read(root.join("foreign.stdout")).unwrap(),
                b"foreign replacement sentinel"
            );
            assert_eq!(
                fs::read(root.join("foreign.saved")).unwrap(),
                b"complete raw stdout"
            );
        } else {
            assert!(error.contains("owned raw cleanup incomplete"), "{error}");
            let stage = fs::read_dir(&root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".asb-evidence-stage-")
                })
                .unwrap();
            assert!(error.contains(stage.to_str().unwrap()), "{error}");
            assert_eq!(
                fs::read(stage.join("content")).unwrap(),
                b"complete raw stdout"
            );
        }
        assert!(!root.join(format!("{name}.stderr")).exists());
        assert!(root.join("run.json").is_file());
        assert!(root.join("summary.json").is_file());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(unix)]
fn fixture_measurement() -> RunMeasurement<'static> {
    RunMeasurement {
        planning: None,
        effective_budgets: None,
        execution: None,
        recording: None,
        observation: Default::default(),
        execution_plan_fingerprint: None,
        module: None,
        benchmark_outcome: None,
        replica_index: Some(1),
        execution_cohort: None,
        schema_version: 2,
        run_id: "immutable-grade".into(),
        status: crate::runner::disposition::RunStatus::Completed,
        success: false,
        process_success: true,
        workload_success: Some(false),
        workload_resource_disposition: None,
        workload_artifacts: &[],
        failure_kind: None,
        failure_reason: None,
        success_rate: Some(0.0),
        adapter_smoke_success: None,
        agent: "fixture",
        workload: "fixture",
        task: None,
        workload_evaluation: "bundled-fixture",
        workload_baseline_passed: Some(false),
        workload_baseline: None,
        workload_failure: None,
        provider: "fixture",
        model: "fixture",
        elapsed_ms: 17,
        exit_code: Some(0),
        token_usage: TokenUsage::default(),
        steps: Some(1),
        tool_call_count: 0,
        tool_calls: &[],
        started_unix_ms: 0,
    }
}

#[cfg(unix)]
pub(crate) fn stopped_fixture(session: &mut Session, name: &str) {
    struct Adapter;
    impl collectors::CollectorAdapter for Adapter {
        fn id(&self) -> &'static str {
            "capture-fixture"
        }
        fn process(
            &self,
            session: &mut Session,
            context: &collectors::ProcessContext<'_>,
        ) -> Result<collectors::ProcessData, String> {
            if context.name == "foreign" {
                fs::rename(
                    session.dir.join("foreign.stdout"),
                    session.dir.join("foreign.saved"),
                )
                .unwrap();
                fs::write(
                    session.dir.join("foreign.stdout"),
                    b"foreign replacement sentinel",
                )
                .unwrap();
            }
            let mut data = collectors::ProcessData::from_output(context);
            if context.name == "admission" {
                let full = crate::collectors::read_complete_output(context.stdout_reader()?)?;
                let value = full
                    .trim()
                    .strip_prefix("A=")
                    .unwrap()
                    .parse::<f64>()
                    .unwrap();
                data.metrics.insert("value".into(), value);
            }
            Ok(data)
        }
    }
    static ADAPTER: Adapter = Adapter;
    let stdout_path = session.dir.join(format!("{name}.stdout"));
    let stderr_path = session.dir.join(format!("{name}.stderr"));
    let captures = [
        crate::evidence::Capture::create(&stdout_path).unwrap(),
        crate::evidence::Capture::create(&stderr_path).unwrap(),
    ];
    let script = if name == "admission" {
        "printf 'A=17\\n'; printf 'complete raw stderr' >&2"
    } else {
        "printf 'complete raw stdout'; printf 'complete raw stderr' >&2"
    };
    let mut child = Command::new("sh")
        .args(["-c", script])
        .stdout(std::process::Stdio::from(captures[0].writer().unwrap()))
        .stderr(std::process::Stdio::from(captures[1].writer().unwrap()))
        .spawn()
        .unwrap();
    let status = child.wait().unwrap();
    session.stopped_collectors.push(StoppedCollector {
        collector: Collector {
            name: name.into(),
            adapter: &ADAPTER,
            producer: Producer::completed(status),
            started: Instant::now(),
            captures,
        },
        completion: ProducerCompletion::test_completed(status),
        elapsed_ms: 1,
    });
}

#[cfg(unix)]
fn collector_raw_case(point: crate::evidence::FaultPoint) {
    let root = std::env::temp_dir().join(format!(
        "asb-raw-publication-{}",
        crate::tools::runtime::unique_id()
    ));
    let mut session = Session::start_with_collectors(&root, &[], None).unwrap();
    session
        .event(collectors::LifecycleEvent::RunStarted)
        .unwrap();
    stopped_fixture(&mut session, "first");
    stopped_fixture(&mut session, "sibling");
    let scope = crate::evidence::tests::Scope::new(root.join("first.json"), Some(point), vec![]);
    let error = session.finish(fixture_measurement()).unwrap_err();
    assert!(scope.hits() > 0);
    assert_eq!(
        fs::read(root.join("first.stdout")).unwrap(),
        b"complete raw stdout",
        "failed publication consumed raw stdout"
    );
    assert_eq!(
        fs::read(root.join("first.stderr")).unwrap(),
        b"complete raw stderr"
    );
    assert!(
        error.contains(root.join("first.stdout").to_str().unwrap()),
        "{error}"
    );
    assert!(root.join("sibling.json").is_file());
    assert!(!root.join("sibling.stdout").exists());
    assert!(!root.join("sibling.stderr").exists());
    for file in [
        "run.json",
        "lifecycle.json",
        "components.json",
        "summary.json",
    ] {
        let _: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join(file)).unwrap()).unwrap();
    }
    let run: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("run.json")).unwrap()).unwrap();
    assert_eq!(run["workload_success"], false);
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn failed_collector_result_keeps_raw_sources_and_finishes_siblings() {
    collector_raw_case(crate::evidence::FaultPoint::Rename);
}

#[cfg(unix)]
#[test]
fn unconfirmed_collector_result_keeps_raw_sources_and_finishes_siblings() {
    collector_raw_case(crate::evidence::FaultPoint::DirectorySync);
}

#[test]
fn partial_measurement_serialization_does_not_clobber_previous_file() {
    struct Broken;
    impl Serialize for Broken {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut map = serializer.serialize_map(Some(2))?;
            map.serialize_entry("partial", &true)?;
            Err(S::Error::custom("deliberate encoding failure"))
        }
    }
    let root = std::env::temp_dir().join(format!(
        "asb-measurement-publish-{}",
        crate::tools::runtime::unique_id()
    ));
    fs::create_dir(&root).unwrap();
    let path = root.join("run.json");
    let old = b"{\"official_grade\":false}";
    fs::write(&path, old).unwrap();
    let result = write_json(&path, &Broken);
    let bytes = fs::read(&path).unwrap();
    fs::remove_dir_all(root).unwrap();
    assert!(result.is_err());
    assert_eq!(
        bytes, old,
        "failed serialization clobbered prior measurement"
    );
}

#[test]
fn malformed_summary_history_is_preserved_not_reset() {
    let root = std::env::temp_dir().join(format!(
        "asb-summary-publish-{}",
        crate::tools::runtime::unique_id()
    ));
    fs::create_dir(&root).unwrap();
    let path = root.join("summary.json");
    let old = b"{\"runs\":[{\"official_grade\":true}], broken history";
    fs::write(&path, old).unwrap();
    let result = update_summary_value(&root, serde_json::json!({"workload_success":false}));
    let bytes = fs::read(&path).unwrap();
    fs::remove_dir_all(root).unwrap();
    assert!(
        result.is_err(),
        "malformed history was silently accepted and replaced"
    );
    assert_eq!(bytes, old);
}

#[test]
fn summary_transaction_retains_unknown_fields_and_rejects_invalid_runs() {
    let root = std::env::temp_dir().join(format!(
        "asb-summary-history-{}",
        crate::tools::runtime::unique_id()
    ));
    fs::create_dir(&root).unwrap();
    let path = root.join("summary.json");
    let old = serde_json::json!({"schema_version":2,"future":{"retained":17},"runs":[{"future_kind":"extension","native_reward":0.75,"resolved":null}]});
    fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    update_summary_value(&root, serde_json::json!({"workload_success":false,"benchmark_outcome":{"evaluation":{"outcome":"pending"}}})).unwrap();
    let updated: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(updated["future"], old["future"]);
    assert_eq!(updated["runs"][0], old["runs"][0]);
    assert_eq!(updated["runs"].as_array().unwrap().len(), 2);
    for old in [
        b"{}".as_slice(),
        b"{\"runs\":null}".as_slice(),
        b"{\"runs\":[1]}".as_slice(),
        b"[]".as_slice(),
    ] {
        fs::write(&path, old).unwrap();
        assert!(update_summary_value(&root, serde_json::json!({})).is_err());
        assert_eq!(fs::read(&path).unwrap(), old);
    }
    fs::remove_dir_all(root).unwrap();
}
