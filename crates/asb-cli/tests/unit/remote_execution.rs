//! Protocol-phase fixtures, NOT qualification of runtime/SDK installation.
//! Production execution always constructs PreparedBootstrap through real prefetch.
use super::*;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/remote-execution-fixtures")
            .join(TaskIdentity::local("protocol", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn prepared(&self) -> PreparedBootstrap {
        PreparedBootstrap {
            binary: "fixture-asb-not-executed".into(),
            tools_root: self.0.join("tools"),
            installation: crate::install::GuestBootstrap::fixture_empty(),
        }
    }
    fn session(&self, events: &[Value], cleanup: &str) -> Session {
        let script = r#"
import json,sys
from pathlib import Path
events=json.loads(sys.argv[1]); log=Path(sys.argv[2]); cleanup=sys.argv[3]
def record(op):
 with log.open('a') as stream: stream.write(op+'\n')
def closed():
 record('cancel');print(json.dumps(dict(event='closed',cleanup=cleanup)),flush=True)
raw=sys.stdin.readline()
if not raw: closed();sys.exit(0)
request=json.loads(raw);assert request['op']=='agent';record('agent')
assert request['request']['config']['concurrency']==1
assert request['request']['config']['workspace']=='/fixture-task'
assert request['bootstrap']==dict(runtime_tools=[],assets=[],public_package_cache=False)
for event in events:
 print(json.dumps(event),flush=True)
 if event['event']=='paused':
  raw=sys.stdin.readline()
  if not raw: closed();sys.exit(0)
  assert json.loads(raw)==dict(op='resume',exec_id=event['target']['exec_id']);record('resume')
for raw in sys.stdin:
 request=json.loads(raw);record(request['op'])
 if request['op']=='evaluate':
  print(json.dumps(dict(event='evaluated',result=dict(workload='fixture',task_id='task',dataset=None,outcome='unresolved',resolved=False))),flush=True)
 elif request['op']=='close':
  print(json.dumps(dict(event='closed',cleanup=cleanup)),flush=True);sys.exit(0)
closed()
"#;
        let mut command = Command::new("python3");
        command
            .args(["-I", "-S", "-B", "-u", "-c", script])
            .arg(serde_json::to_string(events).unwrap())
            .arg(self.0.join("operations"))
            .arg(cleanup);
        Session {
            process_lifetime: super::super::session::ProcessLifetime::Quiesce,
            controller: Controller::spawn(&mut command).unwrap(),
            invocation: TaskInvocation {
                identity: TaskIdentity::local("fixture", Some("task")),
                prompt: "fixture task".into(),
                workspace: self.0.join("workspace"),
            },
            baseline: BaselineEvidence::Unavailable,
            guest_workspace: "/fixture-task".into(),
            agent_timeout: Duration::from_secs(1),
            measurement_active: false,
            preparation_active: false,
            cleanup_attempted: false,
            evaluation_completed: false,
            artifacts: self.0.join("attempt"),
        }
    }
    fn operations(&self) -> Vec<String> {
        fs::read_to_string(self.0.join("operations"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn configuration() -> crate::benchmark::BenchmarkConfig {
    toml::from_str(
        "agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='fixture'\nconcurrency=4\n",
    )
    .unwrap()
}
fn connection() -> crate::endpoint::Connection {
    serde_json::from_value(json!({"interface":"open_ai_chat","authentication":"bearer",
        "base_url":"http://127.0.0.1:1/v1","credential":"fixture-not-a-real-key"}))
    .unwrap()
}
fn completed() -> Value {
    json!({"event":"completed","result":{"exit_code":7,"stdout":"b2s=","stderr":"ZGlhZw==",
        "elapsed_seconds":0.125,"metadata":{"schema_version":1,"exit_code":7,"final_response":"fixture",
            "token_usage":{},"steps":2,"execution_failure":null,"tree_quiesced":true}}})
}

#[test]
fn prepared_protocol_preserves_output_exit_time_and_grades_only_after_completion() {
    for show_output in [false, true] {
        let fixture = Fixture::new();
        let events = [
            json!({"event":"paused","target":{"exec_id":"fixture"}}),
            json!({"event":"output","data":"aGk="}),
            json!({"event":"execution_ended"}),
            completed(),
        ];
        let mut session = fixture.session(&events, "removed");
        let config = configuration();
        let project = crate::project::ProjectConfig::default();
        let connection = connection();
        let mut context = ExecutionContext {
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: true,
            measurements: None,
            show_output,
        };
        let mut output = Vec::new();
        session.begin_agent_preparation(&mut context).unwrap();
        let result =
            session.run_prepared(&mut context, &mut output, &connection, fixture.prepared());
        let result = session.settle_execution(&mut context, result).unwrap();
        assert_eq!(result.output.status.code(), Some(7));
        assert_eq!(result.output.stdout, b"ok");
        assert_eq!(result.output.stderr, b"diag");
        assert_eq!(result.elapsed_ms, Some(125));
        assert_eq!(result.metadata.steps, Some(2));
        assert_eq!(output, if show_output { b"hi".to_vec() } else { vec![] });
        assert!(matches!(
            session.evaluate("ignored").unwrap(),
            Evaluation::Failed(_)
        ));
        session.finish().unwrap();
        assert_eq!(
            fixture.operations(),
            ["agent", "resume", "evaluate", "close"]
        );
        assert_eq!(
            config.concurrency, 4,
            "guest projection must not mutate host config"
        );
    }
}

#[test]
fn protocol_errors_preserve_original_reason_and_cancel_instead_of_grading() {
    let bad_metadata = json!({"event":"completed","result":{"metadata":{}}});
    let mut live_tree = completed();
    live_tree["result"]["metadata"]["tree_quiesced"] = json!(false);
    for (event, reason) in [
        (
            json!({"event":"unknown"}),
            "unexpected isolated execution event",
        ),
        (
            json!({"event":"output","data":"!"}),
            "invalid bounded output encoding",
        ),
        (bad_metadata, "invalid agent result metadata"),
        (live_tree, "process tree was not confirmed stopped"),
    ] {
        let fixture = Fixture::new();
        let mut session = fixture.session(&[event], "stopped-preserved");
        let config = configuration();
        let project = crate::project::ProjectConfig::default();
        let connection = connection();
        let mut context = ExecutionContext {
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: false,
            measurements: None,
            show_output: false,
        };
        let result = session.run_prepared(
            &mut context,
            &mut Vec::new(),
            &connection,
            fixture.prepared(),
        );
        let Err(error) = session.settle_execution(&mut context, result) else {
            panic!("invalid protocol accepted");
        };
        assert!(error.contains(reason), "{error}");
        assert_eq!(fixture.operations(), ["agent", "cancel"]);
        assert!(!session.evaluation_completed);
    }
}

struct BrokenOutput;
impl Write for BrokenOutput {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("fixture output failure"))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn broken_output_and_deadline_use_same_preserving_error_envelope() {
    for timeout in [false, true] {
        let fixture = Fixture::new();
        let events = if timeout {
            vec![]
        } else {
            vec![json!({"event":"output","data":"aGk="})]
        };
        let mut session = fixture.session(&events, "stopped-preserved");
        let config = configuration();
        let project = crate::project::ProjectConfig::default();
        let connection = connection();
        let mut context = ExecutionContext {
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: false,
            measurements: None,
            show_output: true,
        };
        session
            .dispatch_agent(&mut context, &connection, fixture.prepared())
            .unwrap();
        let result = session.receive_execution(
            &mut context,
            &mut BrokenOutput,
            Instant::now() + Duration::from_millis(100),
        );
        let Err(error) = session.settle_execution(&mut context, result) else {
            panic!("fault accepted");
        };
        assert!(
            error.contains(if timeout {
                "response deadline"
            } else {
                "cannot write agent output"
            }),
            "{error}"
        );
        assert_eq!(fixture.operations(), ["agent", "cancel"]);
    }
}

#[test]
fn production_preparation_still_rejects_unregistered_agent_without_protocol_dispatch() {
    let fixture = Fixture::new();
    let mut session = fixture.session(&[], "stopped-preserved");
    let mut config = configuration();
    config.agent = "not-registered".into();
    let project = crate::project::ProjectConfig::default();
    let connection = connection();
    let mut context = ExecutionContext {
        config: &config,
        connection: Some(&connection),
        project_root: &fixture.0,
        project_config: &project,
        replay: false,
        measurements: None,
        show_output: false,
    };
    let result = session.execute(&mut context, &mut Vec::new()).unwrap();
    let Err(error) = result else {
        panic!("production prefetch bypassed");
    };
    assert!(error.contains("no installation adapter"), "{error}");
    assert_eq!(fixture.operations(), ["cancel"]);
    assert!(!crate::project::resolve(&fixture.0, &project.tools_dir).exists());
}

#[test]
fn measured_protocol_rejects_unverified_target_and_execution_end_before_start() {
    use crate::collectors::LifecycleEvent;
    for (event, reason) in [
        (
            json!({"event":"paused","target":{"exec_id":"fixture"}}),
            "invalid identity fields",
        ),
        (
            json!({"event":"execution_ended"}),
            "without an active measured target",
        ),
    ] {
        let fixture = Fixture::new();
        let mut session = fixture.session(&[event], "stopped-preserved");
        let config = configuration();
        let project = crate::project::ProjectConfig::default();
        let connection = connection();
        let measurements_dir = fixture.0.join("measurements");
        let mut measurements = crate::measurements::Session::start_in_execution_namespace(
            &measurements_dir,
            &["agent-memory".into()],
        )
        .unwrap();
        measurements.event(LifecycleEvent::RunStarted).unwrap();
        let mut context = ExecutionContext {
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: true,
            measurements: Some(&mut measurements),
            show_output: false,
        };
        session.begin_agent_preparation(&mut context).unwrap();
        let result = session.run_prepared(
            &mut context,
            &mut Vec::new(),
            &connection,
            fixture.prepared(),
        );
        let Err(error) = session.settle_execution(&mut context, result) else {
            panic!("unverified capture accepted");
        };
        assert!(error.contains(reason), "{error}");
        assert_eq!(fixture.operations(), ["agent", "cancel"]);
        drop(measurements);
        let journal: Value =
            serde_json::from_slice(&fs::read(measurements_dir.join("lifecycle.json")).unwrap())
                .unwrap();
        let kinds = journal
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["event"]["kind"].as_str().unwrap())
            .collect::<Vec<_>>();
        let begin = kinds
            .iter()
            .position(|kind| *kind == "phase_begin")
            .unwrap();
        let failed = kinds
            .iter()
            .position(|kind| *kind == "phase_failed")
            .unwrap();
        assert!(begin < failed);
        assert!(!kinds.contains(&"agent_started"));
        assert!(!kinds.contains(&"agent_execute_begin"));
    }
}

#[test]
fn completed_metadata_retains_unknown_timing_and_requires_valid_service_handoff() {
    for seconds in [Value::Null, json!(-1), json!(86411)] {
        let fixture = Fixture::new();
        let mut event = completed();
        event["result"]["elapsed_seconds"] = seconds;
        let mut session = fixture.session(&[event], "stopped-preserved");
        let config = configuration();
        let project = crate::project::ProjectConfig::default();
        let connection = connection();
        let mut context = ExecutionContext {
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: true,
            measurements: None,
            show_output: false,
        };
        let result = session
            .run_prepared(
                &mut context,
                &mut Vec::new(),
                &connection,
                fixture.prepared(),
            )
            .unwrap();
        assert!(
            result.elapsed_ms.is_none(),
            "unobserved/invalid timing cannot become zero"
        );
        session.cancel().unwrap();
    }
    for mutation in [
        "valid",
        "undeclared",
        "same-namespace",
        "short-token",
        "missing-owner",
    ] {
        let fixture = Fixture::new();
        let mut event = completed();
        event["result"]["metadata"]["tree_quiesced"] = json!(false);
        event["result"]["workload_processes_retained"] = json!(true);
        event["result"]["metadata"]["workload_owner"] =
            json!({"controller_namespace":2,"guest_namespace":3,"token":"a".repeat(32)});
        match mutation {
            "undeclared" => event["result"]["workload_processes_retained"] = json!(false),
            "same-namespace" => {
                event["result"]["metadata"]["workload_owner"]["guest_namespace"] = json!(2)
            }
            "short-token" => event["result"]["metadata"]["workload_owner"]["token"] = json!("a"),
            "missing-owner" => event["result"]["metadata"]["workload_owner"] = Value::Null,
            _ => {}
        }
        let mut session = fixture.session(&[event], "stopped-preserved");
        session.process_lifetime = super::super::session::ProcessLifetime::ThroughEvaluation;
        let config = configuration();
        let project = crate::project::ProjectConfig::default();
        let connection = connection();
        let mut context = ExecutionContext {
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: true,
            measurements: None,
            show_output: false,
        };
        let result = session.run_prepared(
            &mut context,
            &mut Vec::new(),
            &connection,
            fixture.prepared(),
        );
        assert_eq!(result.is_ok(), mutation == "valid", "{mutation}");
        session.cancel().unwrap();
    }
}
