//! Trace-processor protocol fixtures; never start Tracebox or kernel tracing.
use super::*;
use crate::collectors::LifecyclePhase;

struct Fixture(PathBuf, [crate::evidence::Capture; 2]);
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/perfetto-parser-fixtures")
            .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let mut captures = [
            crate::evidence::Capture::create(&root.join("perfetto.stdout")).unwrap(),
            crate::evidence::Capture::create(&root.join("perfetto.stderr")).unwrap(),
        ];
        for capture in &mut captures {
            capture.freeze().unwrap();
        }
        Self(root, captures)
    }
    fn session(&self) -> measurements::Session {
        measurements::Session::start_in_execution_namespace(
            &self.0.join("measurements"),
            &["agent-memory".into()],
        )
        .unwrap()
    }
    #[cfg(unix)]
    fn processor(&self, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = self.0.join("tools/perfetto/bin/trace_processor_shell");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::write_test_file(&self.0, &path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    #[cfg(unix)]
    fn successful_processor(&self) -> PathBuf {
        self.processor(r#"[ "$1" = query ] || exit 9
case "$3" in
  *'WITH marks AS'*) printf 'ready,terminal,loss\n1,1,0\n';;
  *"SELECT 'bounds' AS kind"*) printf 'kind,state,io_wait,duration_ns,intervals,threads,start_ts,end_ts,unfinished\nbounds,,-1,0,6,2,1000000,7000000,0\nstate,Running,0,1000000,1,0,0,0,0\nstate,R,0,2000000,1,0,0,0,0\nstate,D,1,3000000,1,0,0,0,0\nstate,D,0,4000000,1,0,0,0,0\nstate,S,0,1000000,1,0,0,0,0\nstate,Z,0,1000000,1,0,0,0,0\n';;
  *'SELECT utid,tid,ts,dur,state'*) printf 'utid,tid,ts,dur,state,io_wait\n1,11,1000000,1000000,Running,0\n1,11,2000000,2000000,R,0\n1,11,4000000,3000000,D,1\n2,12,1000000,4000000,D,0\n2,12,5000000,1000000,S,0\n2,12,6000000,1000000,Z,0\n';;
  *'timestamp_s'*) printf 'timestamp_s,value\n0,10\n1,20\n';;
  *'SELECT COUNT(*) FROM sched '*|*'SELECT COUNT(*) FROM process '*|*'SELECT COUNT(*) FROM thread '*) printf 'count\n7\n';;
  *'SELECT COUNT(*) FROM root CROSS JOIN bounds WHERE'*) printf 'count\n1\n';;
  *'SELECT COUNT(*) FROM slice WHERE'*) printf 'count\n1\n';;
  *) exit 10;;
esac"#)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        #[cfg(unix)]
        crate::fixture_writer::cleanup(&self.0).unwrap();
        #[cfg(not(unix))]
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn csv_series_rejects_invalid_numbers_retains_timestamps_and_bounds_samples() {
    let (times, values) = parse_perfetto_series_output(
        "header,value\njust-one-field\n0,1\n\"1.25\",\"3.5\"\nNaN,2\n2,inf\ninvalid,4\n3,bad\n",
    );
    assert_eq!(times, [0.0, 1250.0]);
    assert_eq!(values, [1.0, 3.5]);
    let output = (0..3000)
        .map(|index| format!("{index},{index}\n"))
        .collect::<String>();
    let (times, values) = parse_perfetto_series_output(&output);
    assert_eq!(times.len(), 2048);
    assert_eq!(values.len(), 2048);
    assert_eq!(times[2047], 2047000.0);
    assert!(perfetto_metric_queries(None, "fixture").is_empty());
    assert!(perfetto_series_queries(None, "fixture").is_empty());
}

#[test]
fn every_lifecycle_marker_uses_session_identity_and_fixed_names() {
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let marker = fixture.0.join("marker");
    session.set_test_marker(&marker);
    let events = [
        (LifecycleEvent::RunStarted, "asb.run.started"),
        (LifecycleEvent::RunFinishing, "asb.run.finishing"),
        (LifecycleEvent::RunFailed, "asb.run.failed"),
        (
            LifecycleEvent::PhaseBegin(LifecyclePhase::ProviderDiscovery),
            "asb.provider.discovery",
        ),
        (
            LifecycleEvent::PhaseEnd(LifecyclePhase::ProviderDiscovery),
            "E",
        ),
        (
            LifecycleEvent::PhaseFailed(LifecyclePhase::ProviderDiscovery),
            "asb.provider.discovery.failed",
        ),
        (LifecycleEvent::MeasurementsReady, "asb.measurements.ready"),
        (LifecycleEvent::AgentSpawn, "asb.agent.spawn"),
        (LifecycleEvent::AgentExecuteBegin, "asb.agent.execute"),
        (LifecycleEvent::AgentStarted, "asb.agent.started"),
        (LifecycleEvent::AgentSpawnFailed, "asb.agent.spawn_failed"),
        (LifecycleEvent::AgentAttachFailed, "asb.agent.attach_failed"),
        (LifecycleEvent::AgentWaitFailed, "asb.agent.wait_failed"),
        (LifecycleEvent::AgentExecuteEnd, "E"),
        (LifecycleEvent::AgentExited, "asb.agent.exited"),
    ];
    let mut previous_len = 0;
    for (event, name) in events {
        ADAPTER.lifecycle_event(&mut session, event).unwrap();
        let content = fs::read_to_string(&marker).unwrap();
        let line = &content[previous_len..];
        previous_len = content.len();
        if name == "E" {
            assert_eq!(line, "E");
        } else {
            assert!(
                line.contains(&format!("{name}.{}", session.trace_id())),
                "{line}"
            );
        }
    }
    assert_eq!(
        session.state::<PerfettoState>().borrow().tracepoint_count,
        events.len() as u64
    );
    ADAPTER.attach_pid(&mut session, 12345).unwrap();
    assert_eq!(session.state::<PerfettoState>().borrow().pid, Some(12345));
    ADAPTER.finish(&mut session).unwrap();
    assert!(
        !session.dir.join("perfetto-tracepoints.json").exists(),
        "disabled adapter must not publish files"
    );
}

#[cfg(unix)]
#[test]
fn complete_capture_processes_metrics_series_and_exact_per_thread_timeline() {
    use std::os::unix::process::ExitStatusExt;
    let fixture = Fixture::new();
    fixture.successful_processor();
    let mut session = fixture.session();
    session.tool_dir = Some(fixture.0.join("tools"));
    let trace = session.dir.join("capture.perfetto-trace");
    fs::write(&trace, b"fixture-native-trace").unwrap();
    {
        let state = session.state::<PerfettoState>();
        let mut state = state.borrow_mut();
        state.enabled = true;
        state.trace = Some(trace);
        state.pid = Some(11);
        state.tracepoint_count = 5;
    }
    let status = std::process::ExitStatus::from_raw(0);
    let result = ADAPTER
        .process(
            &mut session,
            &ProcessContext::new(
                "perfetto",
                &status,
                ["", ""],
                [&fixture.1[0], &fixture.1[1]],
            ),
        )
        .unwrap();
    assert_eq!(result.status, "completed");
    assert_eq!(result.artifact.as_deref(), Some("capture.perfetto-trace"));
    assert_eq!(result.metrics["perfetto.scheduler_events"], 7.0);
    assert_eq!(result.metrics["perfetto.tracepoints"], 5.0);
    assert_eq!(
        result.series["perfetto.scheduler_events_per_s"],
        [10.0, 20.0]
    );
    assert_eq!(
        result.timestamps["perfetto.scheduler_events_per_s"],
        [0.0, 1000.0]
    );
    ADAPTER.finish(&mut session).unwrap();
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(session.dir.join("perfetto-tracepoints.json")).unwrap())
            .unwrap();
    assert_eq!(report["capture_complete"], true);
    assert_eq!(report["marker_verified"], true);
    let svg = fs::read_to_string(session.dir.join("perfetto-state-timeline.svg")).unwrap();
    for label in [
        "tid 11",
        "tid 12",
        "running: 1000000 ns",
        "runnable: 2000000 ns",
        "I/O wait: 3000000 ns",
        "uninterruptible sleep: 4000000 ns",
        "interruptible sleep: 1000000 ns",
        "other scheduler state: 1000000 ns",
    ] {
        assert!(svg.contains(label), "{label}");
    }
    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(session.dir.join("perfetto-state-summary.json")).unwrap())
            .unwrap();
    assert_eq!(summary["metrics"]["running_ms"], 1.0);
    assert_eq!(summary["metrics"]["io_wait_ms"], 3.0);
    assert!(
        session
            .dir
            .join("perfetto-metric-scheduler_events.svg")
            .is_file()
    );
}

#[cfg(unix)]
#[test]
fn failed_capture_and_ambiguous_scope_never_publish_unscoped_scheduler_metrics() {
    use std::os::unix::process::ExitStatusExt;
    let fixture = Fixture::new();
    fixture.processor("printf '1,0,1\\n'");
    let mut session = fixture.session();
    session.tool_dir = Some(fixture.0.join("tools"));
    let trace = session.dir.join("capture.perfetto-trace");
    fs::write(&trace, b"incomplete").unwrap();
    {
        let state = session.state::<PerfettoState>();
        let mut state = state.borrow_mut();
        state.enabled = true;
        state.trace = Some(trace);
        state.pid = Some(11);
    }
    let status = std::process::ExitStatus::from_raw(15);
    let result = ADAPTER
        .process(
            &mut session,
            &ProcessContext::new(
                "perfetto",
                &status,
                ["", ""],
                [&fixture.1[0], &fixture.1[1]],
            ),
        )
        .unwrap();
    assert_eq!(result.status, "failed");
    assert_eq!(result.metrics.len(), 2);
    assert!(result.series.is_empty());
    ADAPTER.finish(&mut session).unwrap();
    assert!(
        fs::read_to_string(session.dir.join("perfetto-state-timeline.svg"))
            .unwrap()
            .contains("incomplete or lossy")
    );
    let result = ADAPTER
        .process(
            &mut session,
            &ProcessContext::new(
                "perfetto",
                &status,
                ["", "FtraceController unavailable"],
                [&fixture.1[0], &fixture.1[1]],
            ),
        )
        .unwrap();
    assert_eq!(result.status, "unavailable");
    session
        .state::<PerfettoState>()
        .borrow_mut()
        .capture_complete = true;
    fixture.processor("printf '0\\n'");
    let metrics = session.perfetto_trace_metrics();
    assert_eq!(metrics.len(), 2);
    assert!(
        session
            .state::<PerfettoState>()
            .borrow()
            .scope_warning
            .as_ref()
            .unwrap()
            .contains("Cannot uniquely identify")
    );
    fixture.processor("exit 2");
    assert!(session.perfetto_trace_series().0.is_empty());
    let result = session.perfetto_trace_metrics();
    assert_eq!(result.len(), 2);
}

#[cfg(unix)]
#[test]
fn timeline_notices_handle_missing_failed_empty_and_malformed_processor_output() {
    let fixture = Fixture::new();
    let trace = fixture.0.join("trace");
    fs::write(&trace, b"fixture").unwrap();
    let missing = fixture.0.join("missing");
    write_state_timeline_svg(&fixture.0, &missing, &trace, 11, "fixture").unwrap();
    assert!(
        fs::read_to_string(fixture.0.join("perfetto-state-timeline.svg"))
            .unwrap()
            .contains("processor is missing")
    );
    for (body, reason) in [
        (
            "exit 2",
            "cannot aggregate complete Perfetto scheduler states",
        ),
        (
            "printf 'invalid,row\\n1,not-a-timestamp,1,R,0\\n'",
            "invalid Perfetto state aggregate row",
        ),
    ] {
        let processor = fixture.processor(body);
        write_state_timeline_svg(&fixture.0, &processor, &trace, 11, "fixture").unwrap();
        assert!(
            fs::read_to_string(fixture.0.join("perfetto-state-timeline.svg"))
                .unwrap()
                .contains(reason)
        );
    }
    assert!(
        write_timeline_notice(&fixture.0.join("missing"), "unavailable")
            .unwrap_err()
            .contains("cannot write")
    );
    let mut session = fixture.session();
    assert!(session.perfetto_trace_metrics().is_empty());
    assert!(session.perfetto_trace_series().0.is_empty());
    session.state::<PerfettoState>().borrow_mut().trace = Some(trace);
    session
        .state::<PerfettoState>()
        .borrow_mut()
        .capture_complete = true;
    assert_eq!(session.perfetto_trace_metrics().len(), 2);
    assert!(session.perfetto_trace_series().0.is_empty());
}

#[test]
fn oversized_trace_retains_base_metrics_without_starting_post_processing() {
    let fixture = Fixture::new();
    let trace = fixture.0.join("oversized.perfetto-trace");
    let file = fs::File::create(&trace).unwrap();
    file.set_len(MAX_TRACE_PROCESSOR_INPUT_BYTES + 1).unwrap();
    let warning = analysis_budget_warning(&trace).unwrap();
    assert!(warning.contains("complete native trace is retained"));

    let mut session = fixture.session();
    let state = session.state::<PerfettoState>();
    let mut state = state.borrow_mut();
    state.trace = Some(trace);
    state.capture_complete = true;
    state.tracepoint_count = 3;
    state.analysis_warning = Some(warning);
    drop(state);
    let metrics = session.perfetto_trace_metrics();
    assert_eq!(metrics["perfetto.tracepoints"], 3.0);
    assert!(metrics.contains_key("perfetto.trace_bytes"));
    assert!(session.perfetto_trace_series().0.is_empty());
}

#[cfg(unix)]
#[test]
fn oversized_trace_skips_processor_during_collector_processing() {
    use std::os::unix::process::ExitStatusExt;

    let fixture = Fixture::new();
    let trace = fixture.0.join("oversized-processing.perfetto-trace");
    fs::File::create(&trace)
        .unwrap()
        .set_len(MAX_TRACE_PROCESSOR_INPUT_BYTES + 1)
        .unwrap();
    let invoked = fixture.0.join("processor-invoked");
    fixture.processor(&format!("touch {}", invoked.display()));
    let mut session = fixture.session();
    session.tool_dir = Some(fixture.0.join("tools"));
    session.state::<PerfettoState>().borrow_mut().trace = Some(trace);
    let status = std::process::ExitStatus::from_raw(0);
    let result = ADAPTER
        .process(
            &mut session,
            &ProcessContext::new(
                "perfetto",
                &status,
                ["", ""],
                [&fixture.1[0], &fixture.1[1]],
            ),
        )
        .unwrap();
    assert_eq!(result.metrics["perfetto.tracepoints"], 0.0);
    assert!(!invoked.exists());
    let state = session.state::<PerfettoState>();
    let state = state.borrow();
    assert!(!state.capture_complete);
    assert!(state.analysis_warning.is_some());
    assert!(state.capture_warning.is_some());
}

#[cfg(unix)]
#[test]
fn missing_capture_context_remains_diagnostic_and_owned_svg_errors_propagate() {
    use std::os::unix::process::ExitStatusExt;
    let fixture = Fixture::new();
    let mut session = fixture.session();
    ADAPTER.start(&mut session).unwrap(); // disabled: no service acquisition
    session.set_test_marker(&fixture.0.join("marker"));
    assert!(
        session
            .verify_perfetto_marker_capture()
            .unwrap_err()
            .contains("did not produce a trace")
    );
    let status = std::process::ExitStatus::from_raw(0);
    let result = ADAPTER
        .process(
            &mut session,
            &ProcessContext::new(
                "perfetto",
                &status,
                ["", ""],
                [&fixture.1[0], &fixture.1[1]],
            ),
        )
        .unwrap();
    assert_eq!(result.status, "failed");
    assert!(result.metrics.is_empty());
    assert!(
        session
            .state::<PerfettoState>()
            .borrow()
            .marker_warning
            .as_ref()
            .unwrap()
            .contains("did not produce")
    );
    let trace = session.dir.join("capture.perfetto-trace");
    fs::write(&trace, b"fixture").unwrap();
    session.state::<PerfettoState>().borrow_mut().trace = Some(trace);
    assert!(
        session
            .verify_perfetto_marker_capture()
            .unwrap_err()
            .contains("processor is unavailable")
    );
    let error = write_perfetto_metric_svgs(&fixture.0.join("missing"), None, 0, &BTreeMap::new())
        .unwrap_err();
    assert!(error.contains("cannot write Perfetto metric SVG"));
    write_perfetto_metric_svgs(&fixture.0, None, 12, &BTreeMap::new()).unwrap();
    let svg = fs::read_to_string(fixture.0.join("perfetto-metric-tracepoints.svg")).unwrap();
    assert!(svg.contains("native trace unavailable"));
    assert!(svg.contains("lifecycle markers: 12"));
    assert_eq!(ADAPTER.id(), "perfetto");
    assert_eq!(ADAPTER.environment_flag(), Some("ASB_MEASURE_PERFETTO"));
    assert_eq!(ADAPTER.tool_id(), Some("perfetto"));
    assert!(ADAPTER.requires_pid());
    assert!(!ADAPTER.stop_at_execution_end());
    assert!(ADAPTER.producer_policy("perfetto").graceful.is_some());
    assert!(ADAPTER.prioritizes_artifact("perfetto-state-timeline.svg"));
    assert!(ADAPTER.prioritizes_artifact("perfetto-metric-events.svg"));
    assert!(!ADAPTER.prioritizes_artifact("foreign.svg"));
    assert!(
        ADAPTER
            .display_artifact("perfetto-state-timeline.svg")
            .is_some()
    );
    assert!(
        ADAPTER
            .display_artifact("perfetto-metric-events.svg")
            .is_some()
    );
    assert!(
        ADAPTER
            .display_artifact("perfetto-metric-events.json")
            .is_none()
    );
}
