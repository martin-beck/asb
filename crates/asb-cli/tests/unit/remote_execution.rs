//! Protocol-phase fixtures, NOT qualification of runtime/SDK installation.
//! Production execution always constructs PreparedBootstrap through real prefetch.
use super::*;

#[test]
fn material_request_and_response_share_the_same_absolute_deadline() {
    let mut command = ControllerCommand::new("python3").unwrap();
    command.args(["-I", "-S", "-B", "-u", "-c", "import json,sys\nrequest=json.loads(sys.stdin.readline())\nassert request['op']=='initial-material'\nassert isinstance(request['binary'],str)\nassert sys.stdin.read()==''\n"]);
    let mut controller = Controller::spawn(&mut command).unwrap();
    let started = Instant::now();
    let error =
        request_initial_material_until(&mut controller, started + Duration::from_millis(100))
            .unwrap_err();
    assert!(error.contains("deadline"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1));
    controller
        .complete_response(Instant::now() + Duration::from_secs(1))
        .unwrap();
}
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
        self.session_with_grade(events, cleanup, json!(false))
    }
    fn session_with_grade(&self, events: &[Value], cleanup: &str, resolved: Value) -> Session {
        let script = r#"
import json,sys
from pathlib import Path
events=json.loads(sys.argv[1]); log=Path(sys.argv[2]); cleanup=sys.argv[3]; resolved=json.loads(sys.argv[4])
def record(op):
 with log.open('a') as stream: stream.write(op+'\n')
def closed():
 record('cancel');print(json.dumps(dict(event='closed',cleanup=cleanup)),flush=True)
raw=sys.stdin.readline()
if not raw: closed();sys.exit(0)
request=json.loads(raw);assert request['op']=='agent';record('agent')
log.with_name('preparation-seconds').write_text(json.dumps(request['preparation_seconds']))
assert request['request']['config']['concurrency']==1
assert request['request']['config']['workspace']=='/fixture-task'
assert request['timeout']==1
assert request['request']['timeout_seconds']==1
assert request['bootstrap']==dict(runtime_tools=[],assets=[],public_package_cache=False,npm_package_tree=None)
for event in events:
 print(json.dumps(event),flush=True)
 if event['event']=='paused':
  raw=sys.stdin.readline()
  if not raw: closed();sys.exit(0)
  assert json.loads(raw)==dict(op='resume',exec_id=event['target']['exec_id']);record('resume')
for raw in sys.stdin:
 request=json.loads(raw);record(request['op'])
 if request['op']=='evaluate':
  print(json.dumps(dict(event='evaluated',result=dict(workload='fixture',task_id='task',dataset=None,outcome='unresolved',resolved=resolved))),flush=True)
 elif request['op']=='close':
  print(json.dumps(dict(event='closed',cleanup=cleanup)),flush=True);sys.exit(0)
closed()
"#;
        let mut command = ControllerCommand::new("python3").unwrap();
        command
            .args(["-I", "-S", "-B", "-u", "-c", script])
            .arg(serde_json::to_string(events).unwrap())
            .arg(self.0.join("operations"))
            .arg(cleanup)
            .arg(serde_json::to_string(&resolved).unwrap());
        self.with_controller(Controller::spawn(&mut command).unwrap())
    }
    fn with_controller(&self, controller: Controller) -> Session {
        Session {
            evaluation_seconds: EVALUATION_TIMEOUT_SECONDS,
            preparation_seconds: WORKLOAD_READY_TIMEOUT.as_secs(),
            task_default_execution_seconds: 1.0,
            process_lifetime: super::super::session::ProcessLifetime::Quiesce,
            controller,
            invocation: TaskInvocation {
                identity: TaskIdentity::local("fixture", Some("task")),
                input: "fixture task".into(),
                workspace: self.0.join("workspace"),
            },
            baseline: BaselineEvidence::Unavailable,
            initial_material: Err(
                "legacy execution fixture did not request initial material".into()
            ),
            guest_workspace: "/fixture-task".into(),
            agent_timeout: Duration::from_secs(1),
            measurement_active: false,
            preparation_active: false,
            cleanup_attempted: false,
            completed_grade: None,
            settlement: None,
            artifacts: self.0.join("attempt"),
        }
    }
    fn actual_task_server(&self, grades: &[Value]) -> Session {
        self.actual_task_server_observation(grades, &json!({}), 7)
    }
    fn actual_task_server_observation(
        &self,
        grades: &[Value],
        observation: &Value,
        carrier: i32,
    ) -> Session {
        // Actual Python task-loop producer paired with the actual Rust transport.
        // Only the SDK/agent implementation is an inert, process-local fixture.
        let script = r#"
import json,sys,types
from pathlib import Path
sys.path.insert(0,sys.argv[1])
from asb_task_server import serve
from asb_execution import ExecutionError
log=Path(sys.argv[2]);grades=json.loads(sys.argv[3]);observation=json.loads(sys.argv[4]);carrier=int(sys.argv[5])
def record(op):
 with log.open('a') as output: output.write(op+'\n')
def execute_agent(task,request,attach,output,cancelled,on_ended,**options):
 record('agent')
 assert request['op']=='agent' and request['timeout']==1
 assert request['request']['config']['workspace']=='/fixture-task'
 attach(dict(exec_id='owned-execution'));record('resume')
 output('stdout',b'live\x00output')
 result=dict(exit_code=carrier,stdout=b'complete\x00output',stderr=b'diagnostic',elapsed_seconds=.125,
  metadata=dict(schema_version=1,exit_code=carrier,final_response='fixture',token_usage={},steps=2,tree_quiesced=True))
 result['metadata'].update(observation)
 on_ended(result)
 return result
guest=types.ModuleType('asb_guest');guest.execute_agent=execute_agent;guest.PreparationBudget=lambda seconds:None;sys.modules['asb_guest']=guest
class Task:
 invocation=dict(identity=dict(workload='fixture',task_id='task',attempt_id='owned-attempt',dataset=None),prompt='fixture issue',workspace='/fixture-task')
 cleanup_status='pending'
 def __enter__(self): return self
 def __exit__(self,*args):
  record('preserve' if self.interrupted else 'normal-close')
  self.cleanup_status='stopped-preserved' if self.interrupted else 'removed'
 def evaluate(self,timeout):
  record('evaluate');assert timeout==1800
  result=grades.pop(0)
  if result.get('raise_error'): raise ExecutionError('fixture evaluator failed')
  return result
raise SystemExit(serve(Task(),sys.stdin,sys.stdout))
"#;
        let mut command = ControllerCommand::new("python3").unwrap();
        command
            .args(["-I", "-S", "-B", "-u", "-c", script])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/benchmarks"))
            .arg(self.0.join("operations"))
            .arg(serde_json::to_string(grades).unwrap())
            .arg(serde_json::to_string(observation).unwrap())
            .arg(carrier.to_string());
        let mut controller = Controller::spawn(&mut command).unwrap();
        let ready = controller
            .receive(Instant::now() + Duration::from_secs(3))
            .unwrap();
        assert_eq!(EventKind::of(&ready), Some(EventKind::Ready));
        self.with_controller(controller)
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
fn actual_python_producer_and_rust_session_keep_both_grades_and_consume_close_permission() {
    #[derive(Default)]
    struct ProgressSpy {
        updates: usize,
        stdout: Vec<u8>,
        finished: bool,
    }
    impl crate::workloads::session::ExecutionProgress for ProgressSpy {
        fn update(&mut self, stdout: &[u8], _: &[u8]) {
            self.updates += 1;
            self.stdout = stdout.into();
        }
        fn finish(&mut self) {
            self.finished = true;
        }
    }
    for resolved in [true, false] {
        let fixture = Fixture::new();
        let grade = json!({"workload":"fixture","task_id":"task","dataset":null,
            "outcome":if resolved {"resolved"} else {"unresolved"},"resolved":resolved});
        let mut session = fixture.actual_task_server(&[grade]);
        let config = configuration();
        let project = crate::project::ProjectConfig::default();
        let connection = connection();
        let mut progress = ProgressSpy::default();
        let mut context = ExecutionContext {
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: true,
            tape_cassette: None,
            measurements: None,
            show_output: true,
            progress: Some(&mut progress),
            launch: None,
        };
        let mut output = Vec::new();
        let result =
            session.run_prepared(&mut context, &mut output, &connection, fixture.prepared());
        let result = session.settle_execution(&mut context, result).unwrap();
        assert_eq!(output, b"live\x00output");
        assert!(progress.updates > 0);
        assert_eq!(progress.stdout, b"live\x00output");
        assert!(!progress.finished);
        assert_eq!(result.output.stdout, b"complete\x00output");
        assert_eq!(result.output.status.code(), Some(7));
        assert_eq!(result.elapsed_ms, Some(125));
        assert_eq!(
            matches!(session.evaluate("").unwrap(), Evaluation::Passed),
            resolved
        );
        assert!(session.completed_grade.is_some());
        session.finish().unwrap();
        assert!(session.completed_grade.is_none());
        assert_eq!(
            fixture.operations(),
            ["agent", "resume", "evaluate", "normal-close"]
        );
    }
}

#[test]
fn actual_python_task_loop_forwards_declared_termination_without_changing_grade_or_settlement() {
    // These are declared protocol facts, not actual signal/exit executions.
    // Real code137/143 versus signal9/15 are qualified by the guest/gate tests.
    use crate::process_termination::Observation;
    for (observation, carrier) in [
        (
            json!({"process_termination":{"kind":"code","code":137}}),
            137,
        ),
        (
            json!({"process_termination":{"kind":"code","code":143}}),
            143,
        ),
        (
            json!({"process_termination":{"kind":"signal","signal":9}}),
            1,
        ),
        (
            json!({"process_termination":{"kind":"signal","signal":15}}),
            1,
        ),
        (json!({}), 1),
        (
            json!({"process_termination":{"kind":"signal","signal":0}}),
            1,
        ),
    ] {
        for resolved in [Some(true), Some(false), None] {
            let fixture = Fixture::new();
            let grade = json!({"workload":"fixture","task_id":"task","dataset":null,
                "outcome":match resolved {Some(true)=>"resolved",Some(false)=>"unresolved",None=>"pending"},
                "resolved":resolved});
            let mut session =
                fixture.actual_task_server_observation(&[grade], &observation, carrier);
            let config = configuration();
            let project = crate::project::ProjectConfig::default();
            let connection = connection();
            let mut context = ExecutionContext {
                config: &config,
                connection: Some(&connection),
                project_root: &fixture.0,
                project_config: &project,
                replay: true,
                tape_cassette: None,
                measurements: None,
                show_output: false,
                progress: None,
                launch: None,
            };
            let result = session.run_prepared(
                &mut context,
                &mut Vec::new(),
                &connection,
                fixture.prepared(),
            );
            let result = session.settle_execution(&mut context, result).unwrap();
            assert!(result.metadata.tree_quiesced);
            assert_eq!(
                result.metadata.observation,
                Observation::from_record(&observation)
            );
            assert_eq!(result.output.status.code(), Some(carrier));
            let evaluated = session.evaluate("");
            match resolved {
                Some(true) => assert!(matches!(evaluated.unwrap(), Evaluation::Passed)),
                Some(false) => assert!(matches!(evaluated.unwrap(), Evaluation::Failed(_))),
                None => assert!(evaluated.is_err()),
            }
            assert_eq!(session.completed_grade.is_some(), resolved.is_some());
            let settlement = session.finish().unwrap();
            assert_eq!(
                settlement.disposition,
                if resolved.is_some() {
                    ResourceDisposition::Removed
                } else {
                    ResourceDisposition::StoppedPreserved
                }
            );
            assert_eq!(session.finish().unwrap(), settlement);
            assert_eq!(
                fixture.operations(),
                [
                    "agent",
                    "resume",
                    "evaluate",
                    if resolved.is_some() {
                        "normal-close"
                    } else {
                        "preserve"
                    }
                ]
            );
        }
    }
}

#[test]
fn actual_python_late_bad_grade_or_error_revokes_close_and_preserves_instead() {
    let good = json!({"workload":"fixture","task_id":"task","dataset":null,"outcome":"unresolved","resolved":false});
    let mut bad_task = good.clone();
    bad_task["task_id"] = json!("other");
    let mut bad_dataset = good.clone();
    bad_dataset["dataset"] = json!({"source":"other","revision":"other","split":"other"});
    let mut contradiction = good.clone();
    contradiction["resolved"] = json!(true);
    let mut unknown = good.clone();
    unknown["outcome"] = json!("future");
    let mut pending = good.clone();
    pending["outcome"] = json!("pending");
    let mut absent = good.clone();
    absent.as_object_mut().unwrap().remove("resolved");
    for bad in [
        bad_task,
        bad_dataset,
        contradiction,
        unknown,
        pending,
        absent,
        json!({"raise_error":true}),
    ] {
        let fixture = Fixture::new();
        let mut session = fixture.actual_task_server(&[good.clone(), bad]);
        assert!(matches!(
            session.evaluate("").unwrap(),
            Evaluation::Failed(_)
        ));
        assert!(session.completed_grade.is_some());
        assert!(session.evaluate("").is_err());
        assert!(session.completed_grade.is_none());
        assert_eq!(
            session.finish().unwrap().disposition,
            ResourceDisposition::StoppedPreserved
        );
        assert_eq!(fixture.operations(), ["evaluate", "evaluate", "preserve"]);
    }
}

#[test]
fn unmeasured_shared_launch_requires_valid_target_and_aborts_before_cancellation() {
    use super::super::session::LaunchReadiness;
    use std::cell::Cell;
    struct Launch<'a> {
        fixture: &'a Fixture,
        fail_ready: bool,
        ready: Cell<usize>,
        aborted: Cell<bool>,
    }
    impl LaunchReadiness for Launch<'_> {
        fn ready(&self) -> Result<(), String> {
            self.ready.set(self.ready.get() + 1);
            if self.fail_ready {
                Err("sibling preparation failed".into())
            } else {
                Ok(())
            }
        }
        fn abort(&self) {
            assert!(
                !self.fixture.operations().iter().any(|op| op == "cancel"),
                "peers must be notified before namespace cancellation"
            );
            self.aborted.set(true);
        }
        fn remaining(&self) -> Result<Duration, String> {
            Ok(Duration::from_secs(1800))
        }
    }
    for fault in [
        "none",
        "bad-target",
        "missing-target",
        "preparation-failed",
        "ready-failure",
    ] {
        let fixture = Fixture::new();
        let mut target = json!({"container_id":"a".repeat(64),"exec_id":"b".repeat(64),
            "namespace_pid":42,"daemon_pid":null,"pid_scope":"container-namespace"});
        if fault == "bad-target" {
            target["exec_id"] = json!("invalid");
        }
        let events = match fault {
            "missing-target" => vec![completed()],
            "preparation-failed" => vec![
                json!({"event":"preparation_failed"}),
                json!({"event":"error","error_class":"ExecutionError","reason":"selected agent runtime installation failed; inspect private install artifacts"}),
            ],
            _ => vec![json!({"event":"paused","target":target}), completed()],
        };
        let mut session = fixture.session(&events, "stopped-preserved");
        let config = configuration();
        let project = crate::project::ProjectConfig::default();
        let connection = connection();
        let launch = Launch {
            fixture: &fixture,
            fail_ready: fault == "ready-failure",
            ready: Cell::new(0),
            aborted: Cell::new(false),
        };
        let mut context = ExecutionContext {
            launch: Some(&launch),
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: true,
            tape_cassette: None,
            measurements: None,
            show_output: false,
            progress: None,
        };
        let result = session.run_prepared(
            &mut context,
            &mut Vec::new(),
            &connection,
            fixture.prepared(),
        );
        let result = session.settle_execution(&mut context, result);
        assert_eq!(result.is_ok(), fault == "none", "{fault}");
        if fault == "preparation-failed" {
            assert!(
                result
                    .as_ref()
                    .err()
                    .unwrap()
                    .contains("selected agent runtime installation failed")
            );
        }
        assert_eq!(
            launch.ready.get(),
            usize::from(matches!(fault, "none" | "ready-failure")),
            "{fault}"
        );
        assert_eq!(launch.aborted.get(), fault != "none", "{fault}");
        assert_eq!(
            fs::read_to_string(fixture.0.join("preparation-seconds")).unwrap(),
            "1800.0",
            "shared preparation budget must cross the typed request unchanged"
        );
        if fault == "none" {
            session.cancel().unwrap();
        }
        assert_eq!(
            fixture.operations(),
            if fault == "none" {
                vec!["agent", "resume", "cancel"]
            } else {
                vec!["agent", "cancel"]
            }
        );
    }
}

#[test]
fn completed_guest_survives_capture_end_errors_and_still_grades_then_closes() {
    for failed_event in [
        crate::collectors::LifecycleEvent::AgentExecuteEnd,
        crate::collectors::LifecycleEvent::AgentExited,
    ] {
        let fixture = Fixture::new();
        let mut session = fixture.session(
            &[json!({"event":"execution_ended"}), completed()],
            "removed",
        );
        // Private protocol seam represents the already verified/resumed target;
        // no PID, namespace or installation checks are bypassable in production.
        session.measurement_active = true;
        let mut measurements = crate::measurements::Session::start_with_collectors(
            &fixture.0.join("measurements"),
            &[],
            None,
        )
        .unwrap();
        measurements.test_event_failure = Some(failed_event);
        let config = configuration();
        let project = crate::project::ProjectConfig::default();
        let connection = connection();
        let mut context = ExecutionContext {
            launch: None,
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: true,
            tape_cassette: None,
            measurements: Some(&mut measurements),
            show_output: false,
            progress: None,
        };
        let result = session.run_prepared(
            &mut context,
            &mut Vec::new(),
            &connection,
            fixture.prepared(),
        );
        let result = session.settle_execution(&mut context, result).unwrap();
        assert_eq!(result.output.stdout, b"ok");
        assert_eq!(result.output.status.code(), Some(7));
        assert_eq!(result.elapsed_ms, Some(125));
        assert_eq!(result.collector_errors.len(), 1);
        assert!(result.collector_errors[0].contains("fixture collector event failure"));
        assert!(!session.measurement_active);
        assert!(matches!(
            session.evaluate("ignored").unwrap(),
            Evaluation::Failed(_)
        ));
        session.finish().unwrap();
        assert_eq!(fixture.operations(), ["agent", "evaluate", "close"]);
    }
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
        let caps = session.effective_budgets();
        assert_eq!(caps.execution_seconds, Some(1.0));
        assert_eq!(caps.task_default_execution_seconds, Some(1.0));
        assert_eq!(caps.evaluation_seconds, Some(EVALUATION_TIMEOUT_SECONDS));
        assert_eq!(caps.agent_preparation_seconds, None);
        let config = configuration();
        let project = crate::project::ProjectConfig::default();
        let connection = connection();
        let mut context = ExecutionContext {
            launch: None,
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: true,
            tape_cassette: None,
            measurements: None,
            show_output,
            progress: None,
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
fn unknown_grade_settlement_never_repeats_evaluation_or_cancellation() {
    let fixture = Fixture::new();
    let mut session = fixture.session_with_grade(&[completed()], "stopped-preserved", Value::Null);
    let config = configuration();
    let project = crate::project::ProjectConfig::default();
    let connection = connection();
    let mut context = ExecutionContext {
        launch: None,
        config: &config,
        connection: Some(&connection),
        project_root: &fixture.0,
        project_config: &project,
        replay: true,
        tape_cassette: None,
        measurements: None,
        show_output: false,
        progress: None,
    };
    session.begin_agent_preparation(&mut context).unwrap();
    let result = session.run_prepared(
        &mut context,
        &mut Vec::new(),
        &connection,
        fixture.prepared(),
    );
    session.settle_execution(&mut context, result).unwrap();
    assert!(session.evaluate("").is_err());
    let settlement = session.finish().unwrap();
    assert_eq!(
        settlement.disposition,
        crate::workloads::ResourceDisposition::StoppedPreserved
    );
    assert_eq!(session.finish().unwrap(), settlement);
    drop(session);
    assert_eq!(fixture.operations(), ["agent", "evaluate", "cancel"]);
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
            launch: None,
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: false,
            tape_cassette: None,
            measurements: None,
            show_output: false,
            progress: None,
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
        assert!(session.completed_grade.is_none());
        assert_eq!(
            session.finish().unwrap().disposition,
            ResourceDisposition::StoppedPreserved
        );
        assert_eq!(
            session.finish().unwrap().disposition,
            ResourceDisposition::StoppedPreserved
        );
        assert_eq!(fixture.operations(), ["agent", "cancel"]);
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
fn broken_human_output_does_not_cancel_a_completed_guest_submission_or_grader() {
    let fixture = Fixture::new();
    let mut session = fixture.session(
        &[
            json!({"event":"output","data":"aGk="}),
            json!({"event":"output","data":"YWdhaW4="}),
            completed(),
        ],
        "removed",
    );
    let config = configuration();
    let project = crate::project::ProjectConfig::default();
    let connection = connection();
    let mut context = ExecutionContext {
        launch: None,
        config: &config,
        connection: Some(&connection),
        project_root: &fixture.0,
        project_config: &project,
        replay: true,
        tape_cassette: None,
        measurements: None,
        show_output: true,
        progress: None,
    };
    let result = session.run_prepared(
        &mut context,
        &mut BrokenOutput,
        &connection,
        fixture.prepared(),
    );
    let result = session.settle_execution(&mut context, result).unwrap();
    assert_eq!(result.output.stdout, b"ok");
    assert_eq!(result.output.status.code(), Some(7));
    assert!(
        result
            .output_error
            .unwrap()
            .contains("fixture output failure")
    );
    assert!(matches!(
        session.evaluate("ignored").unwrap(),
        Evaluation::Failed(_)
    ));
    session.finish().unwrap();
    assert_eq!(fixture.operations(), ["agent", "evaluate", "close"]);
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
            launch: None,
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: false,
            tape_cassette: None,
            measurements: None,
            show_output: true,
            progress: None,
        };
        session
            .dispatch_agent(
                &mut context,
                &connection,
                fixture.prepared(),
                Instant::now() + Duration::from_secs(3),
            )
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
        launch: None,
        config: &config,
        connection: Some(&connection),
        project_root: &fixture.0,
        project_config: &project,
        replay: false,
        tape_cassette: None,
        measurements: None,
        show_output: false,
        progress: None,
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
            launch: None,
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: true,
            tape_cassette: None,
            measurements: Some(&mut measurements),
            show_output: false,
            progress: None,
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
            launch: None,
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: true,
            tape_cassette: None,
            measurements: None,
            show_output: false,
            progress: None,
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
            launch: None,
            config: &config,
            connection: Some(&connection),
            project_root: &fixture.0,
            project_config: &project,
            replay: true,
            tape_cassette: None,
            measurements: None,
            show_output: false,
            progress: None,
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
