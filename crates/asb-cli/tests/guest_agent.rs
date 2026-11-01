//! Offline guest-driver checks using a registered adapter and fixture executable.
#![cfg(unix)]
#[path = "support/fixture_namespace.rs"]
mod fixture_namespace;
#[cfg(unix)]
#[path = "support/fixture_writer.rs"]
mod fixture_writer;
mod support;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use support::IsolatedEnvironment;

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Project(PathBuf);
impl Project {
    fn new() -> Self {
        let root = fixture_namespace::allocate(&std::env::temp_dir(), "asb-guest-").unwrap();
        for directory in [".asb", ".asb-tools/pi/bin", "workspace"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        std::fs::write(
            root.join(".asb/project.toml"),
            "tools_dir='.asb-tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
        )
        .unwrap();
        let program = root.join(".asb-tools/pi/bin/pi");
        // Mock the adapter's structured final event for this guest plumbing
        // test; real Pi text-mode answer ownership is tested separately.
        fixture_writer::write_file(
            &root,
            &program,
            r#"#!/bin/sh
test -z "$ASB_GUEST_AGENT_REQUEST" || exit 90
test -z "$CODEX_REMOTE_PAYLOAD" || exit 91
test "$ASB_AGENT_API_KEY" = fixture-token || exit 92
printf 'changed\n' > performed.txt
if test -n "$ASB_SESSION_CONTEXT"; then printf '%s' "$ASB_SESSION_CONTEXT" > session-env.txt; fi
if test "$ASB_RENAME_SESSION_CWD" = yes; then mv "$PWD" "$PWD-renamed" || exit 93; fi
printf '%s\n' '{"type":"turn_end","usage":{"input":17,"output":5,"cacheRead":0}}'
printf '%s\n' '{"type":"agent_end","messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"fixture completed"}]}]}'
printf 'fixture diagnostic\n' >&2
exit 7
"#,
        )
        .unwrap();
        std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self(root)
    }
    fn request(&self) -> serde_json::Value {
        serde_json::json!({
            "config": {"agent":"pi", "provider":"openrouter", "model":"fixture-model", "workload":"fixture", "workspace":self.0.join("workspace")},
            "connection": {"interface":"open_ai_chat", "authentication":"bearer", "base_url":"http://127.0.0.1:1/v1", "credential":"fixture-token"},
            "prompt":"offline fixture task", "project_root":self.0,
            "runtime_dir":self.0.join("private"), "run_dir":self.0.join("private/attempt"), "replay":false,
        })
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = fixture_writer::cleanup(&self.0);
    }
}

#[test]
fn guest_refuses_unowned_or_controller_namespace_service_lifetimes_before_preparation() {
    use std::os::unix::fs::MetadataExt;
    let project = Project::new();
    let namespace = std::fs::metadata("/proc/self/ns/pid").unwrap().ino();
    for owner in [
        serde_json::Value::Null,
        serde_json::json!({
            "controller_namespace":namespace,"guest_namespace":namespace,"token":"e".repeat(32)
        }),
    ] {
        let mut request = project.request();
        request["process_lifetime"] = "through-evaluation".into();
        request["workload_owner"] = owner;
        let result = Command::new(env!("CARGO_BIN_EXE_asb"))
            .arg("__asb-agent")
            .isolated_env()
            .env("ASB_GUEST_AGENT_REQUEST", request.to_string())
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        assert!(!project.0.join("private").exists());
        assert!(!project.0.join("workspace/performed.txt").exists());
    }
}

#[test]
fn guest_uses_registered_adapter_prepares_before_gate_and_preserves_output_and_failure() {
    qualify_guest_directory(false, false);
}

#[test]
fn guest_stages_a_declared_mcp_server_through_the_recording_relay_before_launch() {
    let project = Project::new();
    let program = project.0.join(".asb-tools/codex/bin/codex");
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    fixture_writer::write_file(
        &project.0,
        &program,
        "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"result\",\"result\":\"fixture completed\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut request = project.request();
    request["config"]["agent"] = "codex".into();
    request["connection"]["interface"] = "open_ai_responses".into();
    request["config"]["mcp_servers"] = serde_json::json!([{
        "id": "fixture",
        "program": "must-not-be-started-directly",
        "args": ["--fixture"]
    }]);
    request["mcp_mode"] = "Record".into();
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_asb"))
            .arg("__asb-agent")
            .isolated_env()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &project.0)
            .env("ASB_GUEST_AGENT_REQUEST", request.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdout = BufReader::new(process.0.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    if ready.trim() != format!("__ASB_GATE__:{}", process.0.id()) {
        let mut error = String::new();
        process
            .0
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut error)
            .unwrap();
        panic!("guest did not reach its launch gate: {error}");
    }
    let config: toml::Value = toml::from_str(
        &std::fs::read_to_string(
            project
                .0
                .join("private/agent-state/codex/config/config.toml"),
        )
        .unwrap(),
    )
    .unwrap();
    let server = &config["mcp_servers"]["fixture"];
    assert_ne!(
        server["command"].as_str(),
        Some("must-not-be-started-directly")
    );
    assert_eq!(server["args"][0].as_str(), Some("__asb-mcp-relay"));
    assert_eq!(server["args"][1].as_str(), Some("--record-dir"));
    assert_eq!(server["args"][3].as_str(), Some("--"));
    assert_eq!(
        server["args"][4].as_str(),
        Some("must-not-be-started-directly")
    );
    assert!(!project.0.join("private/attempt/mcp-relay/fixture").exists());

    resume_fixture(&mut process);
    let mut output = String::new();
    stdout.read_to_string(&mut output).unwrap();
    assert!(process.0.wait().unwrap().success());
}

#[test]
#[cfg(target_os = "linux")]
fn actual_guest_metadata_preserves_code_versus_signal_without_changing_carrier() {
    for (script, exact, carrier) in [
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
        let project = Project::new();
        fixture_writer::write_file(
            &project.0,
            &project.0.join(".asb-tools/pi/bin/pi"),
            format!("#!/bin/sh\nprintf 'fixture submission\\n'\n{script}\n"),
        )
        .unwrap();
        let mut process = Process(
            Command::new(env!("CARGO_BIN_EXE_asb"))
                .arg("__asb-agent")
                .isolated_env()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", &project.0)
                .env("ASB_GUEST_AGENT_REQUEST", project.request().to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        resume_fixture(&mut process);
        let deadline = Instant::now() + Duration::from_secs(5);
        while process.0.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "guest completion exceeded fixture bound"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let metadata: serde_json::Value = serde_json::from_slice(
            &std::fs::read(project.0.join("private/attempt/agent-result.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["process_termination"], exact);
        assert_eq!(metadata["tree_quiesced"], true);
        assert_eq!(
            metadata["exit_code"],
            if exact["kind"] == "signal" {
                serde_json::Value::Null
            } else {
                serde_json::json!(carrier)
            }
        );
        assert_eq!(process.0.try_wait().unwrap().unwrap().code(), Some(carrier));
    }
}

#[test]
fn guest_preserves_inherited_shell_directory_and_environment_without_changing_project_authority() {
    qualify_guest_directory(true, false);
}

#[test]
fn guest_retains_inherited_directory_identity_after_client_renames_it() {
    qualify_guest_directory(true, true);
}

#[test]
fn guest_refuses_non_utf8_inherited_directory_before_the_application_gate() {
    use std::os::unix::ffi::OsStringExt;
    let project = Project::new();
    let directory = project
        .0
        .join(std::ffi::OsString::from_vec(b"non-utf8-\xff".to_vec()));
    std::fs::create_dir(&directory).unwrap();
    let mut request = project.request();
    request["inherit_working_directory"] = true.into();
    let output = Command::new(env!("CARGO_BIN_EXE_asb"))
        .arg("__asb-agent")
        .current_dir(directory)
        .isolated_env()
        .env("ASB_GUEST_AGENT_REQUEST", request.to_string())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("valid UTF-8"));
    assert!(!project.0.join("private").exists());
}

fn qualify_guest_directory(inherit_directory: bool, rename: bool) {
    let project = Project::new();
    let launch_directory = project.0.join("session-cwd");
    std::fs::create_dir(&launch_directory).unwrap();
    let mut request = project.request();
    request["inherit_working_directory"] = inherit_directory.into();
    request["timeout_seconds"] = serde_json::json!(3600);
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_asb"))
            .arg("__asb-agent")
            .current_dir(&launch_directory)
            .isolated_env()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &project.0)
            .env("ASB_GUEST_AGENT_REQUEST", request.to_string())
            .env("CODEX_REMOTE_PAYLOAD", "must-not-leak")
            .env("ASB_SESSION_CONTEXT", "inherited-context-marker")
            .env("ASB_RENAME_SESSION_CWD", if rename { "yes" } else { "no" })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdout = BufReader::new(process.0.stdout.take().unwrap());
    let mut first = String::new();
    stdout.read_line(&mut first).unwrap();
    assert_eq!(first.trim(), format!("__ASB_GATE__:{}", process.0.id()));
    // Guest adapter state is intentionally isolated beneath the owned
    // agent-state root, never directly under the workload-private root.
    assert!(project.0.join("private/agent-state/pi/config").is_dir());
    assert!(!project.0.join("workspace/performed.txt").exists());
    let deadline = Instant::now() + Duration::from_secs(3);
    while !std::fs::read_to_string(format!("/proc/{}/status", process.0.id()))
        .unwrap()
        .lines()
        .any(|line| line.starts_with("State:") && line.contains('T'))
    {
        assert!(
            Instant::now() < deadline,
            "guest never reached stopped state"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        Command::new("kill")
            .args(["-CONT", &process.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let mut text = String::new();
    stdout.read_to_string(&mut text).unwrap();
    let mut error = String::new();
    process
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut error)
        .unwrap();
    assert_eq!(process.0.wait().unwrap().code(), Some(7), "{error}");
    assert!(text.contains("fixture completed"));
    assert!(!text.contains("fixture diagnostic"));
    assert!(error.contains("fixture diagnostic"));
    let effective = if rename {
        project.0.join("session-cwd-renamed")
    } else if inherit_directory {
        launch_directory
    } else {
        project.0.join("workspace")
    };
    assert!(effective.join("performed.txt").is_file());
    assert_eq!(
        std::fs::read_to_string(effective.join("session-env.txt")).unwrap(),
        "inherited-context-marker"
    );
    if inherit_directory {
        assert!(!project.0.join("workspace/performed.txt").exists());
    }
    let bytes = std::fs::read(project.0.join("private/attempt/agent-result.json")).unwrap();
    let result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        result["execution_workspace"],
        if inherit_directory {
            project.0.join("session-cwd").to_string_lossy().into_owned()
        } else {
            project.0.join("workspace").to_string_lossy().into_owned()
        }
    );
    assert_eq!(result["exit_code"], 7);
    // The guest's interim result precedes collection of the owned summary.
    // A text answer that looks like a turn event is not usage evidence.
    assert!(result["token_usage"]["input_tokens"].is_null());
    assert!(result["token_usage"]["output_tokens"].is_null());
    assert!(result["token_usage"]["total_tokens"].is_null());
    assert_eq!(result["final_response"], "fixture completed");
    assert!(!String::from_utf8_lossy(&bytes).contains("fixture-token"));
}

#[test]
fn guest_preserves_adapter_artifact_steps_and_usage() {
    let project = Project::new();
    let program = project.0.join(".asb-tools/hermes/bin/hermes");
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    fixture_writer::write_file(
        &project.0,
        &program,
        r#"#!/bin/sh
while test "$#" -gt 0; do
    if test "$1" = --usage-file; then
        shift
        printf '%s\n' '{"input_tokens":31,"output_tokens":11,"api_calls":4}' > "$1"
        printf 'fixture answer\n'
        exit 0
    fi
    shift
done
exit 93
"#,
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut request = project.request();
    request["config"]["agent"] = "hermes".into();
    request["connection"]["interface"] = "open_ai_responses".into();
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_asb"))
            .arg("__asb-agent")
            .isolated_env()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &project.0)
            .env("ASB_GUEST_AGENT_REQUEST", request.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdout = BufReader::new(process.0.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(ready.trim(), format!("__ASB_GATE__:{}", process.0.id()));
    let deadline = Instant::now() + Duration::from_secs(3);
    while !std::fs::read_to_string(format!("/proc/{}/status", process.0.id()))
        .unwrap()
        .lines()
        .any(|line| line.starts_with("State:") && line.contains('T'))
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        Command::new("kill")
            .args(["-CONT", &process.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let mut text = String::new();
    stdout.read_to_string(&mut text).unwrap();
    assert!(process.0.wait().unwrap().success());
    let result: serde_json::Value = serde_json::from_slice(
        &std::fs::read(project.0.join("private/attempt/agent-result.json")).unwrap(),
    )
    .unwrap();
    // The isolated guest exports its adapter-owned frozen evidence.  The host
    // controller reconstructs that receipt and lets Hermes derive public
    // steps/usage after the boundary; the private guest result must not claim
    // those controller-owned fields itself.
    assert_eq!(result["steps"], serde_json::Value::Null);
    assert_eq!(
        result["token_usage"]["total_tokens"],
        serde_json::Value::Null
    );
    let receipt = result["exported_artifacts"]
        .as_array()
        .and_then(|receipts| receipts.first())
        .expect("one deferred adapter collection receipt");
    assert_eq!(receipt["kind"], "asb-deferred-adapter-collection");
    let artifact = project
        .0
        .join("private/attempt/exports")
        .join(receipt["filename"].as_str().expect("receipt filename"));
    let frozen: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifact).unwrap()).unwrap();
    assert_eq!(frozen["documents"]["usage.json"]["api_calls"], 4);
    assert_eq!(frozen["documents"]["usage.json"]["input_tokens"], 31);
    assert_eq!(frozen["documents"]["usage.json"]["output_tokens"], 11);
    assert!(!project.0.join("private/attempt/hermes/usage.json").exists());
}

#[test]
fn invalid_task_budgets_fail_before_preparing_or_starting_a_client() {
    let project = Project::new();
    for invalid in [-1.0, 0.0, 86401.0] {
        let mut request = project.request();
        request["timeout_seconds"] = serde_json::json!(invalid);
        let output = Command::new(env!("CARGO_BIN_EXE_asb"))
            .arg("__asb-agent")
            .isolated_env()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("ASB_GUEST_AGENT_REQUEST", request.to_string())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("time budget"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-token"));
        assert!(!project.0.join("workspace/performed.txt").exists());
    }
}

#[test]
fn malformed_guest_request_fails_without_echoing_input_or_starting_an_agent() {
    let output = Command::new(env!("CARGO_BIN_EXE_asb"))
        .arg("__asb-agent")
        .isolated_env()
        .env("ASB_GUEST_AGENT_REQUEST", "not-json-sensitive-fixture")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-fixture"));
}

#[cfg(target_os = "linux")]
fn background_agent(project: &Project, hold_streams: bool, code: i32) -> PathBuf {
    let program = project.0.join(".asb-tools/pi/bin/pi");
    let source = format!(
        r#"#!/usr/bin/env python3
import json,os,signal,sys,time
from pathlib import Path
first = os.fork()
if first == 0:
    os.setsid()
    if os.fork() != 0:
        os._exit(0)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    Path('helper.pid').write_text(str(os.getpid()))
    if not {hold_streams}:
        os.close(1)
        os.close(2)
    time.sleep(1.2)
    Path('late-write').write_text('submission changed after client exit')
    os._exit(0)
os.waitpid(first,0)
deadline=time.monotonic()+2
while not Path('helper.pid').exists():
    if time.monotonic()>deadline:
        sys.exit(99)
    time.sleep(.005)
print(json.dumps(dict(type='result',result='fixture completed')),flush=True)
sys.exit({code})
"#,
        hold_streams = if hold_streams { "True" } else { "False" }
    );
    fixture_writer::write_file(&project.0, &program, source).unwrap();
    program
}

#[cfg(target_os = "linux")]
fn resume_fixture(process: &mut Process) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let status = std::fs::read_to_string(format!("/proc/{}/status", process.0.id())).unwrap();
        if status
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('T'))
        {
            break;
        }
        assert!(process.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline, "fixture never stopped at gate");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        Command::new("kill")
            .args(["-CONT", &process.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(target_os = "linux")]
fn assert_helper_quiesced(project: &Project) {
    let pid = std::fs::read_to_string(project.0.join("workspace/helper.pid")).unwrap();
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "helper was not stopped and reaped"
    );
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        !project.0.join("workspace/late-write").exists(),
        "helper changed submission after client exit"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn guest_quiesces_double_forked_helpers_before_returning_any_client_result() {
    let mut unrelated = Process(Command::new("sleep").arg("15").spawn().unwrap());
    for (hold_streams, code) in [(true, 0), (false, 7)] {
        let project = Project::new();
        background_agent(&project, hold_streams, code);
        let mut process = Process(
            Command::new(env!("CARGO_BIN_EXE_asb"))
                .arg("__asb-agent")
                .isolated_env()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &project.0)
                .env("ASB_GUEST_AGENT_REQUEST", project.request().to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut stdout = BufReader::new(process.0.stdout.take().unwrap());
        let mut ready = String::new();
        stdout.read_line(&mut ready).unwrap();
        assert_eq!(ready.trim(), format!("__ASB_GATE__:{}", process.0.id()));
        resume_fixture(&mut process);
        let mut output = String::new();
        stdout.read_to_string(&mut output).unwrap();
        assert_eq!(process.0.wait().unwrap().code(), Some(code));
        let result: serde_json::Value = serde_json::from_slice(
            &std::fs::read(project.0.join("private/attempt/agent-result.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(result["exit_code"], code);
        assert_helper_quiesced(&project);
        assert!(
            unrelated.0.try_wait().unwrap().is_none(),
            "unrelated process was affected"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_gate_stops_and_reaps_adopted_helpers_without_changing_client_failure() {
    let project = Project::new();
    let program = background_agent(&project, true, 7);
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_asb"))
            .arg("__asb-gate")
            .arg(program)
            .current_dir(project.0.join("workspace"))
            .isolated_env()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    resume_fixture(&mut process);
    let mut output = String::new();
    process
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut output)
        .unwrap();
    assert_eq!(process.0.wait().unwrap().code(), Some(7));
    assert_helper_quiesced(&project);
}

#[cfg(target_os = "linux")]
#[test]
fn native_gate_owner_loss_and_cancellation_quiesce_only_owned_descendants() {
    let project = Project::new();
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/native_gate_owner.py"
        ))
        .arg(env!("CARGO_BIN_EXE_asb"))
        .arg(&project.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn tape_owner_loss_stops_its_listener_and_reaps_outbound_helpers() {
    let project = Project::new();
    let output = Command::new("python3")
        .arg("-B")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/proxy_owner.py"))
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/scripts"))
        .arg(&project.0)
        .arg(env!("CARGO_BIN_EXE_asb"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn guest_output_failure_still_quiesces_owned_helpers_without_publishing_a_result() {
    let project = Project::new();
    background_agent(&project, true, 7);
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_asb"))
            .arg("__asb-agent")
            .isolated_env()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &project.0)
            .env("ASB_GUEST_AGENT_REQUEST", project.request().to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdout = BufReader::new(process.0.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(ready.trim(), format!("__ASB_GATE__:{}", process.0.id()));
    // Simulate a failed output consumer while the prepared client starts.
    drop(stdout);
    resume_fixture(&mut process);
    assert!(!process.0.wait().unwrap().success());
    assert!(!project.0.join("private/attempt/agent-result.json").exists());
    assert_helper_quiesced(&project);
}
