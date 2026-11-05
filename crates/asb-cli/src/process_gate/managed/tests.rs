use super::*;
use crate::process_cleanup::test_support;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;

fn root() -> std::path::PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/gate-managed-fixtures")
        .join(crate::workloads::TaskIdentity::local("gate", None).attempt_id);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn measured_resume_rejects_a_pid_outside_the_owned_principal() {
    let scope = test_support::Scope::new();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let (mut command, route) = command(
        &binary,
        "/bin/sh",
        &["-c".into(), "printf principal".into()],
    );
    let mut gate = Admission::with_measured_principal(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    let principal = gate.start_measured_principal().unwrap();
    assert!(gate.resume_measured_principal(std::process::id()).is_err());
    gate.resume_measured_principal(principal).unwrap();
    assert!(gate.wait().unwrap().success());
    let mut output = String::new();
    gate.take_stdout()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert_eq!(output, "principal");
    drop(gate);
    scope.wait_finished();
}

fn command(gate: &Path, program: &str, args: &[String]) -> (Command, crate::launch::CommandRoute) {
    let mut launch = crate::launch::ProcessLaunch::new(program);
    for arg in args {
        launch.arg(arg);
    }
    let (mut command, route) = launch.native(Some(gate));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    (command, route)
}

struct ReleaseOnDrop(std::path::PathBuf);

#[test]
fn cancelled_controller_can_finish_owned_resource_cleanup() {
    let scope = test_support::Scope::new();
    let root = root();
    let ready = root.join("ready");
    let cleaned = root.join("cleaned");
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let script = format!(
        "import os,signal,time\nfrom pathlib import Path\nassert 'ASB_GATE_WORKLOAD_CLEANUP' not in os.environ\ndef cleanup(*_):\n time.sleep(1.2)\n Path({:?}).write_text('stopped')\n raise SystemExit(0)\nsignal.signal(signal.SIGTERM,cleanup)\nPath({:?}).write_text('ready')\nwhile True: time.sleep(.05)\n",
        cleaned.to_str().unwrap(),
        ready.to_str().unwrap()
    );
    let (mut command, route) = command(&binary, "/usr/bin/python3", &["-c".into(), script]);
    let mut gate = Admission::with_workload_controller(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    gate.resume().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = gate.stop().unwrap();
    assert!(
        cleaned.exists(),
        "controller was killed before its owned-resource cleanup finished"
    );
    drop(gate);
    scope.wait_finished();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancelled_controller_cleanup_outlives_callers_wait_budget() {
    let scope = test_support::Scope::new();
    let root = root();
    let ready = root.join("ready");
    let cleaned = root.join("cleaned");
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let script = format!(
        "import signal,time\nfrom pathlib import Path\ndef cleanup(*_):\n time.sleep(7)\n Path({:?}).write_text('stopped')\n raise SystemExit(0)\nsignal.signal(signal.SIGTERM,cleanup)\nPath({:?}).write_text('ready')\nwhile True: time.sleep(.05)\n",
        cleaned.to_str().unwrap(),
        ready.to_str().unwrap()
    );
    let (mut command, route) = command(&binary, "/usr/bin/python3", &["-c".into(), script]);
    let mut gate = Admission::with_workload_controller(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    gate.resume().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        gate.stop().is_err(),
        "cleanup exceeding the caller's budget must not claim completion"
    );
    drop(gate);
    scope.wait_finished();
    assert!(
        cleaned.exists(),
        "retained gate did not let controller cleanup finish"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires Docker and ASB_CANCEL_TEST_IMAGE referring to a locally cached image"]
fn cancelled_controller_stops_its_real_owned_container() {
    use std::process::Command as StdCommand;
    let scope = test_support::Scope::new();
    let root = root();
    let name = format!("asb-cancel-test-{}", std::process::id());
    struct Container(String);
    impl Drop for Container {
        fn drop(&mut self) {
            let _ = StdCommand::new("docker")
                .args(["rm", "-f", &self.0])
                .output();
        }
    }
    let image = std::env::var("ASB_CANCEL_TEST_IMAGE").expect("cached test image");
    let created = StdCommand::new("docker")
        .args([
            "run",
            "-d",
            "--pull=never",
            "--name",
            &name,
            "--label",
            "asb.test=cancel-cleanup",
            &image,
            "sh",
            "-c",
            "trap 'sleep 1; exit 0' TERM; while :; do sleep 0.1; done",
        ])
        .output()
        .unwrap();
    assert!(created.status.success(), "could not start test container");
    let _container = Container(name.clone());
    let ready = root.join("ready");
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let script = format!(
        "import signal,subprocess,time\nfrom pathlib import Path\ndef cleanup(*_):\n subprocess.run(['docker','stop','--time','2',{:?}],check=True,stdout=subprocess.DEVNULL)\n raise SystemExit(0)\nsignal.signal(signal.SIGTERM,cleanup)\nPath({:?}).write_text('ready')\nwhile True: time.sleep(.05)\n",
        name,
        ready.to_str().unwrap()
    );
    let (mut command, route) = command(&binary, "/usr/bin/python3", &["-c".into(), script]);
    let mut gate = Admission::with_workload_controller(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    gate.resume().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = gate.stop().unwrap();
    let inspected = StdCommand::new("docker")
        .args(["inspect", "--format", "{{.State.Running}}", &name])
        .output()
        .unwrap();
    assert!(inspected.status.success());
    assert_eq!(
        String::from_utf8_lossy(&inspected.stdout).trim(),
        "false",
        "owned container was left running"
    );
    drop(gate);
    scope.wait_finished();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn delayed_readiness_checks_operation_deadline_before_continue() {
    let root = root();
    let marker = root.join("must-not-execute");
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let (mut command, route) = command(
        &binary,
        "/bin/sh",
        &["-c".into(), "printf forbidden > must-not-execute".into()],
    );
    command.current_dir(&root);
    let mut gate = Admission::new(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    gate.readiness_delay = Some(Duration::from_millis(300));
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut evaluated = None;
    let resumed = gate
        .resume_when(|| {
            let now = Instant::now();
            evaluated = Some(now);
            now < deadline
        })
        .unwrap();
    assert!(
        !resumed,
        "deadline was checked before slow readiness rather than before CONT"
    );
    assert!(evaluated.unwrap() >= deadline);
    assert!(!marker.exists());
    let _actual_guardian_status = gate
        .stop()
        .expect("no matching quiescence for unresumed guardian");
    assert!(
        !marker.exists(),
        "cancelling the stopped guardian launched the client"
    );
}

#[test]
fn actual_observation_distinguishes_codes_from_signals_without_interruption() {
    use crate::process_termination::ProcessTermination;
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    for (script, exact, carrier) in [
        ("exit 137", ProcessTermination::Code(137), 137),
        ("exit 143", ProcessTermination::Code(143), 143),
        ("kill -KILL $$", ProcessTermination::Signal(9), 1),
        ("kill -TERM $$", ProcessTermination::Signal(15), 1),
    ] {
        let (mut command, route) = command(&binary, "/bin/sh", &["-c".into(), script.into()]);
        let mut gate =
            Admission::with_principal_observation(route.validate_gate(&mut command).unwrap())
                .unwrap()
                .spawn()
                .unwrap();
        gate.wait_stopped().unwrap();
        assert!(
            gate.interrupt_principal(Instant::now() + Duration::from_secs(1))
                .is_err()
        );
        gate.resume().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = gate.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(status.code(), Some(carrier));
        assert_eq!(
            gate.principal_observation().process_termination,
            Some(exact)
        );
        assert!(
            gate.principal_observation()
                .process_termination_diagnostic
                .is_none()
        );
    }
}

#[test]
fn actual_gate_writes_opaque_root_lifecycle_to_its_sealed_trace_channel() {
    let root = root().join(format!(
        "native-trace-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("native-trace")).unwrap();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let (mut command, route) = command(&binary, "/bin/true", &[]);
    let _channel = crate::agents::native_trace::prepare_gate_channel(&mut command, &root).unwrap();
    let mut gate = Admission::new(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    gate.resume().unwrap();
    assert!(gate.wait().unwrap().success());
    let trace = crate::agents::native_trace::Transcript::decode(
        &std::fs::read(root.join("native-trace/native-trace.jsonl")).unwrap(),
    )
    .unwrap();
    assert_eq!(trace.events().len(), 3);
    assert!(
        trace
            .events()
            .iter()
            .all(|event| { event.process == crate::agents::native_trace::ROOT_PROCESS })
    );
    assert_eq!(
        trace
            .events()
            .iter()
            .map(|event| &event.operation)
            .collect::<Vec<_>>(),
        vec![
            &crate::agents::native_trace::Operation::ProcessStart,
            &crate::agents::native_trace::Operation::ProcessExec,
            &crate::agents::native_trace::Operation::ProcessExit,
        ]
    );
    let root_start = trace.events().first().unwrap();
    let request = root_start
        .request
        .as_ref()
        .expect("root start binds the requested executable identity");
    let bytes = std::fs::read("/bin/true").unwrap();
    assert_eq!(request.bytes, bytes.len() as u64);
    assert_eq!(request.sha256, format!("{:x}", Sha256::digest(bytes)));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn actual_gate_records_causal_forks_without_retaining_descendant_ptrace_custody() {
    let root = root().join(format!(
        "native-trace-tree-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("native-trace")).unwrap();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    // This intentionally uses an external program instead of a shell builtin:
    // it must create a child and exec it while the gate is the sole tracer.
    let (mut command, route) = command(
        &binary,
        "/bin/sh",
        &["-c".into(), "/bin/sleep 0.05 & wait".into()],
    );
    let _channel = crate::agents::native_trace::prepare_gate_channel(&mut command, &root).unwrap();
    let mut gate = Admission::new(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    gate.resume().unwrap();
    assert!(gate.wait().unwrap().success());
    let trace = crate::agents::native_trace::Transcript::decode(
        &std::fs::read(root.join("native-trace/native-trace.jsonl")).unwrap(),
    )
    .unwrap();
    let events = trace.events();
    assert_eq!(
        events
            .first()
            .map(|event| (&event.operation, event.process)),
        Some((
            &crate::agents::native_trace::Operation::ProcessStart,
            crate::agents::native_trace::ROOT_PROCESS,
        ))
    );
    let fork = events
        .iter()
        .find(|event| event.operation == crate::agents::native_trace::Operation::ProcessFork)
        .unwrap_or_else(|| panic!("external shell command must fork: {events:#?}"));
    assert_eq!(
        fork.parent_process,
        Some(crate::agents::native_trace::ROOT_PROCESS)
    );
    assert_ne!(fork.process, crate::agents::native_trace::ROOT_PROCESS);
    // The lifecycle backend records the causal fork then immediately detaches
    // the child. Retaining arbitrary helper descendants under ptrace changes
    // their stdio/MCP shutdown behavior; later capsule tracing owns detailed
    // descendant syscall and exit capture without that interference.
    assert!(!events.iter().any(|event| {
        event.process == fork.process
            && matches!(
                event.operation,
                crate::agents::native_trace::Operation::ProcessExec
                    | crate::agents::native_trace::Operation::ProcessExit
            )
    }));
    assert_eq!(
        events.last().map(|event| (&event.operation, event.process)),
        Some((
            &crate::agents::native_trace::Operation::ProcessExit,
            crate::agents::native_trace::ROOT_PROCESS,
        ))
    );
    assert!(events.iter().all(|event| {
        event.response.is_none()
            && event.effects.is_empty()
            && (event.operation == crate::agents::native_trace::Operation::ProcessStart
                || event.request.is_none())
    }));
    assert!(
        events
            .iter()
            .find(|event| event.operation == crate::agents::native_trace::Operation::ProcessStart)
            .and_then(|event| event.request.as_ref())
            .is_some()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn actual_gate_appends_after_a_deterministic_host_trace_event() {
    let root = root().join(format!(
        "native-trace-host-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let trace_directory = root.join("native-trace");
    std::fs::create_dir_all(&trace_directory).unwrap();
    let mut initial = crate::agents::native_trace::Transcript::default();
    initial
        .record(crate::agents::native_trace::Event {
            schema_version: crate::agents::native_trace::SCHEMA_VERSION,
            sequence: 1,
            process: crate::agents::native_trace::ROOT_PROCESS,
            operation: crate::agents::native_trace::Operation::Host,
            parent_process: None,
            request: None,
            response: Some(crate::agents::native_trace::Blob {
                sha256: "0".repeat(64),
                bytes: 1,
            }),
            effects: Vec::new(),
        })
        .unwrap();
    std::fs::write(
        trace_directory.join("native-trace.jsonl"),
        initial.encode().unwrap(),
    )
    .unwrap();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let (mut command, route) = command(&binary, "/bin/true", &[]);
    let _channel = crate::agents::native_trace::prepare_gate_channel(&mut command, &root).unwrap();
    let mut gate = Admission::new(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    gate.resume().unwrap();
    assert!(gate.wait().unwrap().success());
    let trace = crate::agents::native_trace::Transcript::decode(
        &std::fs::read(trace_directory.join("native-trace.jsonl")).unwrap(),
    )
    .unwrap();
    assert_eq!(trace.events().len(), 4);
    assert_eq!(
        trace
            .events()
            .iter()
            .map(|event| &event.operation)
            .collect::<Vec<_>>(),
        vec![
            &crate::agents::native_trace::Operation::Host,
            &crate::agents::native_trace::Operation::ProcessStart,
            &crate::agents::native_trace::Operation::ProcessExec,
            &crate::agents::native_trace::Operation::ProcessExit,
        ]
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn completed_gates_do_not_accumulate_idle_custody_threads() {
    let scope = test_support::Scope::new();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    // A sweep creates many short-lived controller gates.  Every completed
    // admission must join its otherwise-idle custody worker before the next
    // attempt can leave a growing `asb-gate-custody-reaper` thread census.
    for _ in 0..8 {
        let (mut command, route) = command(&binary, "/bin/true", &[]);
        let mut gate = Admission::new(route.validate_gate(&mut command).unwrap())
            .unwrap()
            .spawn()
            .unwrap();
        gate.resume().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while gate.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        drop(gate);
        assert!(
            scope.finished(),
            "idle custody worker survived completed gate"
        );
    }
    assert_eq!(scope.count(), 8);
}

#[test]
fn bad_optional_observations_do_not_hide_matching_base_quiescence() {
    let root = root();
    for kind in [
        "missing",
        "malformed",
        "partial",
        "duplicate",
        "wrong-owner",
    ] {
        let launcher = root.join(kind);
        let payload = match kind {
            "missing" => "b''",
            "malformed" => "b'not-a-control-frame'",
            "partial" => "f(4,0,7)+b'partial'",
            "duplicate" => "f(4,0,7)+f(4,0,7)",
            "wrong-owner" => "f(1,0,123)+f(4,0,7)",
            _ => unreachable!(),
        };
        crate::write_test_file(&root, &launcher,format!("#!/usr/bin/python3\nimport os,signal,socket,struct\nr=socket.socket(fileno=int(os.environ['ASB_GATE_RECEIPT_FD']))\nc=socket.socket(fileno=int(os.environ['ASB_GATE_CONTROL_FD']))\nf=lambda k,d,v:b'ASBCTRL1'+bytes([k,d,0,0])+struct.pack('<I',v)\nc.sendall(f(1,0,os.getpid()))\nr.sendall(b'ASBGATE2'+bytes([1,0,0,0])+struct.pack('<I',os.getpid()))\nos.kill(os.getpid(),signal.SIGSTOP)\nc.sendall({payload}) if {payload} else None\nr.sendall(b'ASBGATE2'+bytes([2,7,0,0])+struct.pack('<I',os.getpid()))\nos._exit(7)\n")).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (mut command, route) = command(&launcher, "/bin/true", &[]);
        let mut gate =
            Admission::with_principal_observation(route.validate_gate(&mut command).unwrap())
                .unwrap()
                .spawn()
                .unwrap();
        gate.resume().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = gate.try_wait().unwrap() {
                assert_eq!(status.code(), Some(7));
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            gate.principal_observation(),
            crate::process_termination::Observation::malformed(),
            "{kind}"
        );
        assert_eq!(gate.stop().unwrap().code(), Some(7));
    }
    std::fs::remove_dir_all(root).unwrap();
}
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, "release fixture");
    }
}

#[test]
fn actual_principal_interrupt_preserves_guardian_until_flush_and_leaves_sentinel_alive() {
    struct Sentinel(Child);
    impl Drop for Sentinel {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut sentinel = Sentinel(Command::new("/bin/sleep").arg("30").spawn().unwrap());
    let root = root();
    let ready = root.join("ready");
    let interrupted = root.join("interrupted");
    let release = root.join("release");
    let flushed = root.join("flushed");
    let release_guard = ReleaseOnDrop(release.clone());
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let script = format!(
        "import os,pathlib,signal,time\nassert 'ASB_GATE_CONTROL_FD' not in os.environ\ndef flush(signum, frame):\n pathlib.Path({interrupted:?}).write_text('interrupt')\n while not pathlib.Path({release:?}).exists(): time.sleep(.005)\n pathlib.Path({flushed:?}).write_text('complete')\n raise SystemExit(7)\nsignal.signal(signal.SIGINT,flush)\npathlib.Path({ready:?}).write_text('ready')\nwhile True: time.sleep(.005)\n"
    );
    let (mut command, route) = command(
        &binary,
        "python3",
        &["-I".into(), "-S".into(), "-c".into(), script],
    );
    let mut gate = Admission::with_principal_control(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    gate.resume().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "principal not ready");
        std::thread::sleep(Duration::from_millis(5));
    }
    gate.interrupt_principal(deadline).unwrap();
    while !interrupted.exists() || !gate.interruption_acknowledged() {
        assert!(gate.try_wait().unwrap().is_none());
        assert!(
            Instant::now() < deadline,
            "principal interrupt not acknowledged"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(gate.principal_exit().is_none());
    assert!(!flushed.exists());
    assert!(sentinel.0.try_wait().unwrap().is_none());
    std::fs::write(&release, "release").unwrap();
    while gate.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "guardian did not settle after flush"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(gate.principal_exit().unwrap().status().code(), Some(7));
    assert_eq!(std::fs::read_to_string(flushed).unwrap(), "complete");
    assert!(sentinel.0.try_wait().unwrap().is_none());
    drop(gate);
    drop(release_guard);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn failed_keeper_creation_never_spawns_a_launcher() {
    let scope = test_support::Scope::new();
    test_support::set_creation_failure(true);
    let (mut command, route) = command(Path::new("/missing"), "/bin/true", &[]);
    let result = Admission::new(route.validate_gate(&mut command).unwrap());
    test_support::set_creation_failure(false);
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("fixture resource exhaustion")
    );
    assert_eq!(scope.count(), 0);
}

#[test]
fn startup_poll_error_transfers_real_child_custody_without_resuming_it() {
    let root = root();
    let launcher = root.join("stopped-startup");
    let marker = root.join("must-not-run");
    crate::write_test_file(
        &root,
        &launcher,
        format!(
            "#!/bin/sh\ntrap '' TERM\nkill -STOP $$\nprintf unsafe > '{}'\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let scope = test_support::Scope::new();
    let (mut command, route) = command(&launcher, "/bin/true", &[]);
    let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
    let mut gate = admission.spawn().unwrap();
    let pid = gate.id();
    let error = gate
        .stop_starting_before(Instant::now() + Duration::from_secs(1), |_| {
            Err(io::Error::other("fixture poll failure"))
        })
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("fixture poll failure") && error.contains("wait-only custody"),
        "{error}"
    );
    assert!(gate.child.is_none());
    drop(gate);
    scope.wait_finished();
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    assert!(!marker.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_stopped_launcher_is_never_resumed_on_admission_failure() {
    let root = root();
    let launcher = root.join("legacy");
    let marker = root.join("must-not-run");
    crate::write_test_file(
        &root,
        &launcher,
        format!(
            "#!/bin/sh\ntrap '' TERM\nkill -STOP $$\nprintf unsafe > '{}'\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let scope = test_support::Scope::new();
    let (mut command, route) = command(&launcher, "/bin/true", &[]);
    let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
    let mut gate = admission.spawn().unwrap();
    let error = gate.resume().unwrap_err().to_string();
    assert!(error.contains("ABI is incompatible"), "{error}");
    assert!(error.contains("rebuild ASB"));
    gate.stop().unwrap();
    drop(gate);
    scope.wait_finished();
    assert!(!marker.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn actual_gate_confirms_nonzero_status_and_hides_its_proof_descriptor() {
    let scope = test_support::Scope::new();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    assert!(
        binary.is_file(),
        "build current ASB binary before this test"
    );
    let args = vec!["-c".into(), "test -z \"${ASB_GATE_RECEIPT_FD+x}\" || exit 88; printf captured; printf diagnostic >&2; exit 7".into()];
    let (mut command, route) = command(&binary, "/bin/sh", &args);
    let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
    let mut gate = admission.spawn().unwrap();
    let mut stdout = gate.take_stdout().unwrap();
    let mut stderr = gate.take_stderr().unwrap();
    gate.resume().unwrap();
    assert_eq!(gate.wait().unwrap().code(), Some(7));
    assert_eq!(gate.try_wait().unwrap().unwrap().code(), Some(7));
    let mut out = String::new();
    let mut err = String::new();
    stdout.read_to_string(&mut out).unwrap();
    stderr.read_to_string(&mut err).unwrap();
    assert_eq!(out, "captured");
    assert_eq!(err, "diagnostic");
    drop(gate);
    scope.wait_finished();
}

#[test]
fn gate_forwards_controlled_stdin_and_eof_to_the_target() {
    let scope = test_support::Scope::new();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let args = vec![
        "-c".into(),
        "IFS= read -r prompt; test \"$prompt\" = controlled-input; test -z \"$(cat)\"".into(),
    ];
    let (mut command, route) = command(&binary, "/bin/sh", &args);
    command.stdin(Stdio::piped());
    let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
    let mut gate = admission.spawn().unwrap();
    gate.take_stdin()
        .unwrap()
        .write_all(b"controlled-input\n")
        .unwrap();
    gate.resume().unwrap();
    assert!(gate.wait().unwrap().success());
    drop(gate);
    scope.wait_finished();
}

#[test]
fn protocol_fixture_exit_without_quiescence_is_never_a_clean_completion() {
    let root = root();
    let launcher = root.join("protocol-only.py");
    crate::write_test_file(&root, &launcher, "#!/usr/bin/python3\nimport os,signal,socket,struct\ns=socket.socket(fileno=int(os.environ['ASB_GATE_RECEIPT_FD']))\ns.sendall(b'ASBGATE2'+bytes([1,0,0,0])+struct.pack('<I',os.getpid()))\nos.kill(os.getpid(),signal.SIGSTOP)\nos._exit(0)\n").unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let scope = test_support::Scope::new();
    let (mut command, route) = command(&launcher, "/bin/true", &[]);
    let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
    let mut gate = admission.spawn().unwrap();
    gate.resume().unwrap();
    let gate_pid = gate.id();
    let error = gate.wait().unwrap_err().to_string();
    assert!(error.contains("without a quiescence receipt"), "{error}");
    assert!(error.contains("role=command"), "{error}");
    assert!(error.contains(&format!("gate_pid={gate_pid}")), "{error}");
    assert!(error.contains("exit=ExitStatus"), "{error}");
    assert!(gate.observed.unwrap().success());
    assert!(gate.stop().unwrap_err().to_string().contains("unconfirmed"));
    assert!(
        gate.child.is_none(),
        "finished child must leave public custody without being signalled again"
    );
    drop(gate);
    scope.wait_finished();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_hello_is_rejected_without_admitting_the_fixture_client() {
    let root = root();
    let launcher = root.join("malformed.py");
    let marker = root.join("must-not-run");
    crate::write_test_file(&root, &launcher, format!("#!/usr/bin/python3\nimport os,signal,socket\ns=socket.socket(fileno=int(os.environ['ASB_GATE_RECEIPT_FD']))\ns.sendall(b'not-compatible!!')\nos.kill(os.getpid(),signal.SIGSTOP)\nopen({:?},'w').write('unsafe')\n",marker.to_str().unwrap())).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let scope = test_support::Scope::new();
    let (mut command, route) = command(&launcher, "/bin/true", &[]);
    let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
    let mut gate = admission.spawn().unwrap();
    assert!(
        gate.resume()
            .unwrap_err()
            .to_string()
            .contains("ABI is incompatible")
    );
    gate.stop().unwrap();
    drop(gate);
    scope.wait_finished();
    assert!(!marker.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn previous_real_abi_hello_is_rejected_before_new_target_wire_can_execute() {
    let root = root();
    let launcher = root.join("previous-abi.py");
    let marker = root.join("must-not-run");
    // ASBGATE1 was the prior valid ABI, not random malformed input. It cannot
    // interpret the new explicit target-argv0 framing and must not get CONT.
    crate::write_test_file(&root, &launcher, format!("#!/usr/bin/python3\nimport os,signal,socket,struct\ns=socket.socket(fileno=int(os.environ['ASB_GATE_RECEIPT_FD']))\ns.sendall(b'ASBGATE1'+bytes([1,0,0,0])+struct.pack('<I',os.getpid()))\nos.kill(os.getpid(),signal.SIGSTOP)\nopen({:?},'w').write('old runtime admitted incompatible arguments')\n",marker.to_str().unwrap())).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let scope = test_support::Scope::new();
    let (mut command, route) = command(&launcher, "/bin/true", &[]);
    let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
    let mut gate = admission.spawn().unwrap();
    assert!(
        gate.resume()
            .unwrap_err()
            .to_string()
            .contains("ABI is incompatible")
    );
    assert!(
        !gate.admitted,
        "malformed Hello must retain Starting cleanup without CONT"
    );
    gate.stop().unwrap();
    drop(gate);
    scope.wait_finished();
    assert!(!marker.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancellation_deadline_retains_a_live_protocol_guardian_until_its_own_finish() {
    let root = root();
    let launcher = root.join("delayed-guardian.py");
    let release = root.join("release");
    let complete = root.join("quiescent");
    // Protocol/keeper fixture only: actual descendant qualification is covered
    // by the native gate integration matrix, not this controllable stand-in.
    crate::write_test_file(&root, &launcher, format!("#!/usr/bin/python3\nimport os,signal,socket,struct,time\nfrom pathlib import Path\nsignal.signal(signal.SIGTERM,signal.SIG_IGN)\ns=socket.socket(fileno=int(os.environ['ASB_GATE_RECEIPT_FD']))\ns.sendall(b'ASBGATE2'+bytes([1,0,0,0])+struct.pack('<I',os.getpid()))\nos.kill(os.getpid(),signal.SIGSTOP)\nwhile not Path({:?}).exists(): time.sleep(.005)\nPath({:?}).write_text('finished under retained custody')\nos._exit(0)\n", release.to_str().unwrap(), complete.to_str().unwrap())).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let scope = test_support::Scope::new();
    let (mut command, route) = command(&launcher, "/bin/true", &[]);
    let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
    let mut gate = admission.spawn().unwrap();
    let _release_on_failure = ReleaseOnDrop(release.clone());
    gate.resume().unwrap();
    let pid = gate.id();
    let error = gate
        .stop_before(Instant::now() + Duration::from_millis(100))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("unconfirmed") && error.contains("wait-only custody"),
        "{error}"
    );
    assert!(gate.child.is_none());
    drop(gate);
    assert!(
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap()
            .lines()
            .all(|line| !line.starts_with("State:") || line.split_whitespace().nth(1) != Some("Z"))
    );
    std::fs::write(&release, "release fixture").unwrap();
    scope.wait_finished();
    assert_eq!(
        std::fs::read_to_string(&complete).unwrap(),
        "finished under retained custody"
    );
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn managed_cancellation_quiesces_real_double_forked_term_resistant_writers() {
    use std::os::fd::{AsRawFd, FromRawFd};
    let scope = test_support::Scope::new();
    let root = root();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let helper_pid = root.join("helper");
    let release = root.join("release");
    let late_write = root.join("must-not-write");
    let script = format!(
        "import os,signal,time\nfrom pathlib import Path\nassert 'ASB_GATE_RECEIPT_FD' not in os.environ\nlinks=[]\nfor p in Path('/proc/self/fd').iterdir():\n try: links.append(os.readlink(p))\n except FileNotFoundError: pass\nassert not any(p.startswith('socket:') for p in links), links\nfirst=os.fork()\nif first == 0:\n os.setsid()\n if os.fork() != 0: os._exit(0)\n signal.signal(signal.SIGTERM,signal.SIG_IGN)\n p=Path({:?}); t=p.with_suffix('.tmp'); t.write_text(str(os.getpid())); t.replace(p)\n while not Path({:?}).exists(): time.sleep(.005)\n Path({:?}).write_text('unsafe')\n time.sleep(30)\n os._exit(0)\nos.waitpid(first,0)\ntime.sleep(30)\n",
        helper_pid.to_str().unwrap(),
        release.to_str().unwrap(),
        late_write.to_str().unwrap()
    );
    let (mut command, route) = command(&binary, "/usr/bin/python3", &["-c".into(), script]);
    let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
    let mut gate = admission.spawn().unwrap();
    gate.resume().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !helper_pid.exists() {
        assert!(
            Instant::now() < deadline,
            "double-forked fixture never started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let pid: u32 = std::fs::read_to_string(&helper_pid)
        .unwrap()
        .parse()
        .unwrap();
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    assert!(fd >= 0);
    let identity = unsafe { std::fs::File::from_raw_fd(fd) };
    assert!(!gate.stop().unwrap().success());
    assert!(gate.receiver.quiescent.is_some());
    let mut event = libc::pollfd {
        fd: identity.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut event, 1, 0) }, 1);
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "confirmed gate left an unreaped helper"
    );
    std::fs::write(&release, "after confirmed quiescence").unwrap();
    assert!(!late_write.exists());
    drop(gate);
    scope.wait_finished();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn verified_stopped_guardian_cancels_without_launching_the_application() {
    let scope = test_support::Scope::new();
    let root = root();
    let marker = root.join("must-not-start");
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let (mut command, route) = command(
        &binary,
        "/bin/sh",
        &[
            "-c".into(),
            "printf unsafe > \"$1\"".into(),
            "fixture".into(),
            marker.to_str().unwrap().into(),
        ],
    );
    let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
    let mut gate = admission.spawn().unwrap();
    gate.wait_stopped().unwrap();
    assert!(
        gate.admitted,
        "validated current gate must retain supervision while waiting for siblings"
    );
    let startup_deadline = gate.startup_deadline;
    gate.wait_stopped().unwrap();
    assert_eq!(
        gate.startup_deadline, startup_deadline,
        "readiness revalidation cannot restart its budget"
    );
    assert!(!marker.exists());
    let pid = gate.id();
    gate.stop().unwrap();
    assert!(gate.receiver.quiescent.is_some());
    assert!(
        !marker.exists(),
        "cancelling a stopped guardian must not start its application"
    );
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    drop(gate);
    scope.wait_finished();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn adopted_descendant_exit_does_not_abort_traced_principal() {
    for exit in ["os._exit(0)", "os.kill(os.getpid(), signal.SIGTERM)"] {
        let root = root().join(format!("adopted-orphan-{}", std::process::id()));
        std::fs::create_dir_all(root.join("native-trace")).unwrap();
        let binary = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("asb");
        let script = format!(
            "import os,time,signal\np=os.fork()\nif p==0:\n q=os.fork()\n if q==0:\n  time.sleep(.1); {exit}\n os._exit(0)\nos.waitpid(p,0)\ntime.sleep(.5)\n"
        );
        let (mut command, route) = command(&binary, "/usr/bin/python3", &["-c".into(), script]);
        let _channel =
            crate::agents::native_trace::prepare_gate_channel(&mut command, &root).unwrap();
        let mut gate =
            Admission::with_principal_observation(route.validate_gate(&mut command).unwrap())
                .unwrap()
                .spawn()
                .unwrap();
        let mut stderr = gate.take_stderr().unwrap();
        gate.resume().unwrap();
        let status = gate.wait().unwrap();
        let mut text = String::new();
        stderr.read_to_string(&mut text).unwrap();
        assert!(
            status.success(),
            "gate failed status={status:?} observation={:?} stderr={text:?}",
            gate.principal_observation()
        );
        assert_eq!(
            gate.principal_observation().process_termination,
            Some(crate::process_termination::ProcessTermination::Code(0))
        );
        let trace = crate::agents::native_trace::Transcript::decode(
            &std::fs::read(root.join("native-trace/native-trace.jsonl")).unwrap(),
        )
        .unwrap();
        assert!(trace.events().iter().any(|event| event.operation
            == crate::agents::native_trace::Operation::ProcessExit
            && event.process == crate::agents::native_trace::ROOT_PROCESS));
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn traced_principal_completion_quiesces_live_adopted_descendant() {
    use std::os::fd::{AsRawFd, FromRawFd};
    let scope = test_support::Scope::new();
    let root = root();
    std::fs::create_dir_all(root.join("native-trace")).unwrap();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let helper = root.join("adopted-helper");
    let release = root.join("release-principal");
    let script = format!(
        "import os,signal,time\nfrom pathlib import Path\np=os.fork()\nif p==0:\n parent=os.getpid()\n q=os.fork()\n if q==0:\n  while os.getppid()==parent: time.sleep(.005)\n  signal.signal(signal.SIGTERM,signal.SIG_IGN)\n  p=Path({:?}); t=p.with_suffix('.tmp'); t.write_text(str(os.getpid())); t.replace(p)\n  while True: time.sleep(1)\n os._exit(0)\nos.waitpid(p,0)\nwhile not Path({:?}).exists(): time.sleep(.005)\n",
        helper.to_str().unwrap(),
        release.to_str().unwrap()
    );
    let (mut command, route) = command(&binary, "/usr/bin/python3", &["-c".into(), script]);
    let _channel = crate::agents::native_trace::prepare_gate_channel(&mut command, &root).unwrap();
    let mut gate =
        Admission::with_principal_observation(route.validate_gate(&mut command).unwrap())
            .unwrap()
            .spawn()
            .unwrap();
    gate.resume().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !helper.exists() {
        assert!(
            Instant::now() < deadline,
            "adopted helper never became ready"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let pid: u32 = std::fs::read_to_string(&helper).unwrap().parse().unwrap();
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    assert!(fd >= 0);
    let identity = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut poll = libc::pollfd {
        fd: identity.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut poll, 1, 0) },
        0,
        "fixture helper must still be alive"
    );
    std::fs::write(&release, "release").unwrap();
    assert!(gate.wait().unwrap().success());
    assert_eq!(
        gate.principal_observation().process_termination,
        Some(crate::process_termination::ProcessTermination::Code(0))
    );
    assert_eq!(
        unsafe { libc::poll(&mut poll, 1, 0) },
        1,
        "gate returned before adopted helper exited"
    );
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "gate returned before adopted helper was reaped"
    );
    drop(gate);
    scope.wait_finished();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn measured_principal_stops_before_exec_and_remains_outside_guardian_helpers() {
    let root = root();
    let marker = root.join("principal-pid");
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let (mut command, route) = command(
        &binary,
        "/bin/sh",
        &["-c".into(), "printf %s $$ > principal-pid".into()],
    );
    command.current_dir(&root);
    let mut gate = Admission::with_measured_principal(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    let principal = gate.start_measured_principal().unwrap();
    assert_ne!(principal, gate.id());
    assert!(!marker.exists());
    assert!(
        Command::new("/bin/kill")
            .args(["-CONT", &principal.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = gate.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        principal.to_string()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancelling_stopped_measured_principal_never_executes_agent() {
    let root = root();
    let marker = root.join("should-not-run");
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let (mut command, route) = command(
        &binary,
        "/bin/sh",
        &["-c".into(), "printf bad > should-not-run".into()],
    );
    command.current_dir(&root);
    let mut gate = Admission::with_measured_principal(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    let principal = gate.start_measured_principal().unwrap();
    let _ = gate.stop().unwrap();
    assert!(!marker.exists());
    assert!(!Path::new(&format!("/proc/{principal}")).exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires working Linux perf process sampling"]
fn measured_principal_perf_inherits_children_but_excludes_guardian_and_sibling() {
    let root = root();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let perf =
        std::env::var_os("ASB_TEST_PERF").expect("set ASB_TEST_PERF to an installed perf binary");
    let code = "import multiprocessing as m,time\ndef f():\n e=time.monotonic()+4\n while time.monotonic()<e: pass\nif __name__=='__main__':\n ps=[m.Process(target=f) for _ in range(4)]\n [p.start() for p in ps]\n open('children','w').write('\\n'.join(str(p.pid) for p in ps))\n [p.join() for p in ps]\n";
    let (mut command, route) = command(&binary, "/usr/bin/python3", &["-c".into(), code.into()]);
    command.current_dir(&root);
    let mut gate = Admission::with_measured_principal(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    let principal = gate.start_measured_principal().unwrap();
    let mut foreign = Command::new("/usr/bin/python3")
        .args([
            "-c",
            "import time; e=time.monotonic()+5\nwhile time.monotonic()<e: pass",
        ])
        .spawn()
        .unwrap();
    let foreign_pid = foreign.id();
    let data = root.join("perf.data");
    let mut recorder = Command::new(&perf)
        .args([
            "record",
            "-e",
            "cpu-clock",
            "-F",
            "19",
            "--inherit",
            "-p",
            &principal.to_string(),
            "-o",
        ])
        .arg(&data)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        recorder.try_wait().unwrap().is_none(),
        "perf failed to attach"
    );
    assert!(
        Command::new("/bin/kill")
            .args(["-CONT", &principal.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = gate.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = Command::new("/bin/kill")
        .args(["-INT", &recorder.id().to_string()])
        .status();
    let _ = recorder.wait();
    let _ = foreign.wait();
    let decoded = Command::new(&perf)
        .args(["script", "--symfs"])
        .arg(&root)
        .args(["-F", "pid,tid", "-i"])
        .arg(&data)
        .output()
        .unwrap();
    assert!(decoded.status.success(), "perf script failed");
    let mut allowed: std::collections::BTreeSet<u32> =
        std::fs::read_to_string(root.join("children"))
            .unwrap()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect();
    allowed.insert(principal);
    let mut observed = std::collections::BTreeSet::new();
    for line in String::from_utf8(decoded.stdout).unwrap().lines() {
        if let Some(pid) = line
            .trim()
            .split('/')
            .next()
            .and_then(|value| value.parse::<u32>().ok())
        {
            observed.insert(pid);
        }
    }
    assert!(!observed.is_empty(), "no perf samples");
    assert!(!observed.contains(&foreign_pid));
    assert!(!observed.contains(&gate.id()));
    assert!(
        observed.is_subset(&allowed),
        "unrelated samples: {observed:?}, allowed: {allowed:?}"
    );
    assert_eq!(
        observed
            .intersection(&allowed)
            .filter(|pid| **pid != principal)
            .count(),
        4,
        "all long-running children must be sampled"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn measured_principal_preserves_native_lifecycle_trace() {
    let root = root();
    std::fs::create_dir_all(root.join("native-trace")).unwrap();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let marker = root.join("principal-pid");
    let (mut command, route) = command(
        &binary,
        "/bin/sh",
        &["-c".into(), "printf %s $$ > principal-pid".into()],
    );
    command.current_dir(&root);
    let _channel = crate::agents::native_trace::prepare_gate_channel(&mut command, &root).unwrap();
    let mut gate = Admission::with_measured_principal(route.validate_gate(&mut command).unwrap())
        .unwrap()
        .spawn()
        .unwrap();
    let principal = gate.start_measured_principal().unwrap();
    assert!(!marker.exists());
    assert!(
        Command::new("/bin/kill")
            .args(["-CONT", &principal.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = gate.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "traced measured agent did not finish"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        principal.to_string()
    );
    let trace = crate::agents::native_trace::Transcript::decode(
        &std::fs::read(root.join("native-trace/native-trace.jsonl")).unwrap(),
    )
    .unwrap();
    assert!(
        trace
            .events()
            .iter()
            .any(|event| event.operation == crate::agents::native_trace::Operation::ProcessStart)
    );
    assert!(
        trace
            .events()
            .iter()
            .any(|event| event.operation == crate::agents::native_trace::Operation::ProcessExec)
    );
    assert!(
        trace
            .events()
            .iter()
            .any(|event| event.operation == crate::agents::native_trace::Operation::ProcessExit)
    );
    std::fs::remove_dir_all(root).unwrap();
}
