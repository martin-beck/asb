//! Generic lifecycle/namespace contracts without a live collector or provider.
use super::*;
use crate::usage::with_artifact_usage;
use collectors::{CollectorAdapter, LifecycleEvent};
use serde_json::{Value, json};

struct Fixture(PathBuf);

#[cfg(target_os = "linux")]
#[test]
fn capture_creation_failure_precedes_any_sampler_or_lifecycle_child() {
    let fixture = Fixture::new();
    let mut session = Session::start_with_collectors(&fixture.0, &[], None).unwrap();
    let sentinel = session.dir.join("agent-memory.stderr");
    fs::write(&sentinel, "previous capture sentinel").unwrap();
    let error = session
        .start_tree_snapshot(
            "agent-memory",
            true,
            Path::new("/bin/false"),
            std::process::id(),
            Box::new(|_| panic!("capture failure must precede worker admission")),
        )
        .unwrap_err();
    assert!(error.contains("cannot create owned raw capture"), "{error}");
    assert!(session.collectors.is_empty());
    assert_eq!(
        fs::read_to_string(&sentinel).unwrap(),
        "previous capture sentinel"
    );
}

#[test]
fn prepared_typed_collector_data_reaches_configure_without_cross_session_sharing() {
    #[derive(Default)]
    struct Prepared {
        directory: PathBuf,
        project_root: Option<PathBuf>,
        configured: bool,
    }
    struct Adapter;
    static ADAPTER: Adapter = Adapter;
    impl CollectorAdapter for Adapter {
        fn id(&self) -> &'static str {
            "fixture-prepared-state"
        }
        fn preflight(&self, context: &mut collectors::PreflightContext<'_>) -> Result<(), String> {
            let state = context.states.state::<Prepared>();
            let mut state = state.borrow_mut();
            state.directory = context.measurement_dir.to_path_buf();
            state.project_root = context.project_root.map(Path::to_path_buf);
            Ok(())
        }
        fn configure(&self, session: &mut Session, _: &[String]) -> Result<(), String> {
            let state = session.state::<Prepared>();
            let mut state = state.borrow_mut();
            assert_eq!(state.directory, session.dir);
            assert_eq!(state.project_root, session.project_root);
            state.configured = true;
            Ok(())
        }
    }
    let first = Fixture::new();
    let second = Fixture::new();
    let mut one = Session::start_with_adapters(
        &first.0,
        &[],
        None,
        None,
        vec![&ADAPTER],
        true,
        Some(&first.0),
    )
    .unwrap();
    let mut two =
        Session::start_with_adapters(&second.0, &[], None, None, vec![&ADAPTER], true, None)
            .unwrap();
    let a = one.state::<Prepared>();
    let b = two.state::<Prepared>();
    assert!(a.borrow().configured && b.borrow().configured);
    assert!(!std::rc::Rc::ptr_eq(&a, &b));
    a.borrow_mut().configured = false;
    assert!(b.borrow().configured);
}

#[cfg(target_os = "linux")]
#[test]
fn process_tree_snapshots_record_owned_target_output_and_command_failures() {
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for executable_exists in [true, false] {
        let fixture = Fixture::new();
        let child = OwnedChild(Command::new("/bin/sleep").arg("30").spawn().unwrap());
        let root = ProcessIdentity::read(child.0.id()).unwrap();
        let output = fixture.0.join("stdout");
        let errors = fixture.0.join("stderr");
        let captures = [
            crate::evidence::Capture::create(&output).unwrap(),
            crate::evidence::Capture::create(&errors).unwrap(),
        ];
        let admitted_output = fixture.0.join("admitted-stdout");
        fs::rename(&output, &admitted_output).unwrap();
        fs::write(&output, "foreign stdout sentinel").unwrap();
        let admitted_errors = fixture.0.join("admitted-stderr");
        fs::rename(&errors, &admitted_errors).unwrap();
        fs::write(&errors, "foreign stderr sentinel").unwrap();
        let snapshot = ProcessTreeSnapshot::start(
            root,
            if executable_exists {
                "/bin/echo".into()
            } else {
                fixture.0.join("missing-sampler")
            },
            Box::new(|pids| {
                assert_eq!(pids.len(), 1);
                SnapshotCommand {
                    arguments: vec!["owned-process-observation".into()],
                    runtime: Duration::from_secs(3),
                    stream_bytes: 1024 * 1024,
                }
            }),
            captures[0].writer().unwrap(),
            captures[1].writer().unwrap(),
        )
        .unwrap();
        let wanted = if executable_exists {
            &admitted_output
        } else {
            &admitted_errors
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while (if executable_exists {
            fs::read_to_string(wanted).map_or(true, |text| text.is_empty())
        } else {
            !snapshot.is_finished()
        }) && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let completion = snapshot.stop();
        assert_eq!(
            fs::read_to_string(&output).unwrap(),
            "foreign stdout sentinel"
        );
        assert_eq!(
            fs::read_to_string(&errors).unwrap(),
            "foreign stderr sentinel"
        );
        let text = fs::read_to_string(wanted).unwrap();
        if executable_exists {
            assert!(completion.is_ok(), "{completion:?}");
            assert!(text.contains("# asb-snapshot-elapsed-seconds"));
            assert!(text.contains("owned-process-observation"));
        } else {
            assert!(
                completion.is_err(),
                "missing executable must fail the observation"
            );
        }
        assert!(
            root.matches(),
            "snapshot must not terminate its observed target"
        );
        drop(child);
    }
}
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/measurement-fixtures")
            .join(crate::workloads::TaskIdentity::local("measurement", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn session(&self) -> Session {
        Session::start_with_adapters(&self.0, &[], None, None, vec![], true, None).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Namespace;
static NAMESPACE: Namespace = Namespace;
#[derive(Default)]
struct NamespaceState {
    attached: Vec<u32>,
    value: Option<Value>,
}
impl CollectorAdapter for Namespace {
    fn id(&self) -> &'static str {
        "fixture-namespace"
    }
    fn prefers_execution_namespace(&self) -> bool {
        true
    }
    fn execution_namespace_requested(&self, _: &mut Session) -> bool {
        true
    }
    fn requires_pid(&self) -> bool {
        true
    }
    fn attach_isolated_pid(&self, session: &mut Session, pid: u32) -> Result<(), String> {
        session
            .state::<NamespaceState>()
            .borrow_mut()
            .attached
            .push(pid);
        Ok(())
    }
    fn export_execution_namespace_state(&self, session: &mut Session) -> Result<Value, String> {
        Ok(session
            .state::<NamespaceState>()
            .borrow()
            .value
            .clone()
            .unwrap_or(Value::Null))
    }
    fn import_execution_namespace_state(
        &self,
        session: &mut Session,
        value: &Value,
    ) -> Result<(), String> {
        session.state::<NamespaceState>().borrow_mut().value = Some(value.clone());
        Ok(())
    }
}

#[test]
fn namespace_round_trip_attaches_only_verified_identity_and_keeps_cpu_units() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    session.adapters.push(&NAMESPACE);
    assert_eq!(
        session.execution_namespace_collectors(),
        ["fixture-namespace"]
    );
    let root = ProcessIdentity::read(std::process::id()).unwrap();
    assert!(
        session
            .attach_isolated_pid(root.pid, root.start_time_ticks + 1)
            .is_err()
    );
    assert!(
        session
            .state::<NamespaceState>()
            .borrow()
            .attached
            .is_empty()
    );
    session
        .attach_isolated_pid(root.pid, root.start_time_ticks)
        .unwrap();
    assert_eq!(
        session.state::<NamespaceState>().borrow().attached,
        [root.pid]
    );
    assert_eq!(session.measurement_target(), Some(root));
    assert_eq!(
        session.sampled_cpu_accounting(),
        None,
        "namespace capture must not receive host process accounting"
    );
    session.record_cpu_accounting(123_000, 456_000, "fixture-exact-accounting");
    assert_eq!(session.sampled_cpu_accounting(), Some((123.0, 456.0)));
    assert_eq!(
        session.component_timings["agent.execute"]
            .cpu_accounting_source
            .as_deref(),
        Some("fixture-exact-accounting")
    );
    let values = BTreeMap::from([(
        "fixture-namespace".into(),
        json!({"schema_version":1,"observations":[2,3]}),
    )]);
    session.import_execution_namespace_state(&values).unwrap();
    assert_eq!(session.export_execution_namespace_state().unwrap(), values);
    assert_eq!(
        session.lifecycle.last().unwrap().event,
        LifecycleEvent::AgentExecuteEnd
    );
}

#[test]
fn lifecycle_population_is_bounded_and_invalid_namespace_selection_fails_before_files() {
    let fixture = Fixture::new();
    let nonexistent = fixture.0.join("not-created");
    for selected in [Vec::new(), vec!["agent_processes".into(); 33]] {
        assert!(Session::start_in_execution_namespace(&nonexistent, &selected).is_err());
    }
    assert!(!nonexistent.exists());
    let mut session = fixture.session();
    for _ in 0..256 {
        session.event(LifecycleEvent::MeasurementsReady).unwrap();
    }
    assert!(
        session
            .event(LifecycleEvent::MeasurementsReady)
            .unwrap_err()
            .contains("too many")
    );
    assert_eq!(session.lifecycle.len(), 256);
    let file = fixture.0.join("not-a-directory");
    fs::write(&file, "block directory creation").unwrap();
    let result =
        Session::start_with_adapters(&file.join("child"), &[], None, None, vec![], true, None);
    let Err(error) = result else {
        panic!("non-directory parent accepted");
    };
    assert!(error.contains("cannot create measurements directory"));
    assert!(error.contains("namei -l"));
}

#[test]
fn unavailable_artifacts_and_bounded_read_failures_never_fabricate_metrics() {
    let fixture = Fixture::new();
    let path = fixture.0.join("missing-tool.json");
    write_unavailable(&path, "fixture", "permission denied by fixture".into()).unwrap();
    let record: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(record["status"], "unavailable");
    assert_eq!(record["stderr"], "permission denied by fixture");
    assert_eq!(record["metrics"], json!({}));
    assert!(record["exit_code"].is_null());
    assert!(crate::evidence::Capture::open(&fixture.0.join("absent")).is_err());
    let file = fixture.0.join("oversized");
    fs::write(&file, vec![b'x'; MAX_CAPTURE + 1]).unwrap();
    assert_eq!(
        crate::evidence::Capture::open(&file)
            .unwrap()
            .diagnostic(MAX_CAPTURE)
            .unwrap()
            .len(),
        MAX_CAPTURE
    );
    assert!(crate::evidence::Capture::open(&fixture.0).is_err());
    assert!(
        check_measurement_directory(&fixture.0.join("missing"))
            .unwrap_err()
            .contains("check disk space")
    );
    assert_eq!(shell_quote("owner's directory"), "owner'\\''s directory");
}

#[test]
fn sudo_command_construction_and_project_entry_points_are_scoped_without_execution() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    session.tool_dir = Some(fixture.0.clone());
    fs::create_dir(fixture.0.join("bin")).unwrap();
    let binary = fixture.0.join("bin/pidstat");
    fs::write(&binary, "not executed").unwrap();
    assert_eq!(session.executable("pidstat"), binary);
    assert_eq!(
        session.executable("unregistered-fixture"),
        PathBuf::from("unregistered-fixture")
    );
    session.sudo_collectors.insert("pidstat".into());
    let command = session.command("pidstat");
    assert_eq!(command.get_program(), "sudo");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["-n".as_ref(), "--".as_ref(), binary.as_os_str()]
    );
}

#[test]
fn incomplete_artifact_usage_fills_missing_fields_without_overwriting_observed_input() {
    let usage = TokenUsage {
        input_tokens: Some(10),
        output_tokens: None,
        source: Some("agent-output".into()),
        ..TokenUsage::default()
    };
    let artifact = TokenUsage {
        output_tokens: Some(3),
        ..TokenUsage::default()
    };
    let competing_input = TokenUsage {
        input_tokens: Some(2),
        ..TokenUsage::default()
    };
    let preserved = with_artifact_usage(usage.clone(), Some(competing_input));
    assert_eq!(preserved.input_tokens, Some(10));
    assert_eq!(preserved.output_tokens, None);
    let combined = with_artifact_usage(usage, Some(artifact));
    assert_eq!(combined.input_tokens, Some(10));
    assert_eq!(combined.output_tokens, Some(3));
    assert_eq!(combined.total_tokens, Some(13));
}

#[test]
fn artifact_evidence_replaces_environment_without_mixing_fields() {
    let environment = TokenUsage {
        input_tokens: Some(10),
        output_tokens: Some(20),
        source: Some("environment".into()),
        ..TokenUsage::default()
    };
    let partial = TokenUsage {
        input_tokens: Some(2),
        ..TokenUsage::default()
    };
    let combined = with_artifact_usage(environment.clone(), Some(partial));
    assert_eq!(
        (
            combined.input_tokens,
            combined.output_tokens,
            combined.total_tokens
        ),
        (Some(2), None, None)
    );
    assert_eq!(combined.source.as_deref(), Some("agent-artifact"));
    let complete = TokenUsage {
        input_tokens: Some(2),
        output_tokens: Some(3),
        ..TokenUsage::default()
    };
    let combined = with_artifact_usage(environment, Some(complete));
    assert_eq!(
        (
            combined.input_tokens,
            combined.output_tokens,
            combined.total_tokens
        ),
        (Some(2), Some(3), Some(5))
    );
    assert_eq!(combined.source.as_deref(), Some("agent-artifact"));
}

#[test]
fn summary_retention_is_bounded_and_unscored_success_is_not_a_resolved_task() {
    let fixture = Fixture::new();
    let path = fixture.0.join("summary.json");
    let max_runs = 10_000;
    let runs = (0..max_runs)
        .map(|index| {
            json!({"run_id":index.to_string(),"workload_evaluation":"unscored",
        "process_success":true,"workload_success":null})
        })
        .collect::<Vec<_>>();
    fs::write(&path, serde_json::to_vec(&json!({"runs":runs})).unwrap()).unwrap();
    let run = RunMeasurement {
        planning: None,
        effective_budgets: None,
        execution: None,
        recording: None,
        observation: Default::default(),
        workload_resource_disposition: None,
        workload_artifacts: &[],
        execution_plan_fingerprint: None,
        module: None,
        benchmark_outcome: None,
        replica_index: None,
        execution_cohort: None,
        schema_version: 2,
        run_id: "latest".into(),
        status: crate::runner::disposition::RunStatus::Unscored,
        success: false,
        process_success: true,
        workload_success: None,
        failure_kind: None,
        failure_reason: None,
        success_rate: None,
        adapter_smoke_success: None,
        agent: "fixture",
        workload: "fixture",
        task: None,
        workload_evaluation: "unscored",
        workload_baseline_passed: None,
        workload_baseline: None,
        workload_failure: None,
        provider: "fixture",
        model: "fixture",
        elapsed_ms: 1,
        exit_code: Some(0),
        token_usage: TokenUsage::default(),
        steps: None,
        tool_call_count: Some(0),
        tool_calls: &[],
        started_unix_ms: 1,
    };
    crate::workloads::summary::update_summary(&fixture.0, serde_json::to_value(&run).unwrap())
        .unwrap();
    let summary: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let runs = summary["runs"].as_array().unwrap();
    assert_eq!(runs.len(), max_runs);
    assert_eq!(runs[0]["run_id"], "1");
    assert_eq!(runs.last().unwrap()["run_id"], "latest");
    assert!(
        runs.last()
            .unwrap()
            .get("process_termination")
            .unwrap()
            .is_null()
    );
    assert_eq!(summary["task_count"], 0);
    assert_eq!(summary["unscored_count"], max_runs);
    assert!(summary["success_rate"].is_null());
}
