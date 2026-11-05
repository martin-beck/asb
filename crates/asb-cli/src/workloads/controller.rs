//! Private, bounded JSON-lines transport to workload-owned execution engines.
#[cfg(test)]
use super::session::ResourceDisposition;
#[cfg(test)]
use super::task_protocol::{CompletedGrade, EventKind};
use serde_json::Value;
use std::io::Write;
#[cfg(test)]
use std::process::Command;
#[cfg(test)]
use std::time::{Duration, Instant};

const MAX_MESSAGE: u64 = 1024 * 1024;

/// Build the typed carrier before applying opaque std::Command configuration.
/// The same command owns env_clear/removals, cwd, stdio and pre_exec through
/// admission. Never reconstruct it from getters to insert a supervisor later.
pub(crate) struct ControllerCommand {
    command: std::process::Command,
    #[cfg(target_os = "linux")]
    route: Option<crate::launch::CommandRoute>,
}

impl ControllerCommand {
    pub(crate) fn new(program: impl AsRef<std::ffi::OsStr>) -> Result<Self, String> {
        Self::from_launch(crate::launch::ProcessLaunch::new(program))
    }

    pub(crate) fn from_launch(launch: crate::launch::ProcessLaunch) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            #[cfg(not(test))]
            let gate = crate::process_gate::executable().map_err(|error| error.to_string())?;
            #[cfg(test)]
            let gate = crate::material_guardian::actual_asb()?;
            let (command, route) = launch.native(Some(&gate));
            Ok(Self {
                command,
                route: Some(route),
            })
        }
        #[cfg(not(target_os = "linux"))]
        Ok(Self {
            command: launch.direct(),
        })
    }
}

impl std::ops::Deref for ControllerCommand {
    type Target = std::process::Command;
    fn deref(&self) -> &Self::Target {
        &self.command
    }
}
impl std::ops::DerefMut for ControllerCommand {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.command
    }
}

pub(crate) fn publish_asset(path: &std::path::Path, source: &str) -> Result<(), crate::Error> {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        if !metadata.is_file() || metadata.len() != source.len() as u64 {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return false;
            }
        }
        std::fs::read(path).is_ok_and(|bytes| bytes == source.as_bytes())
    }) {
        return Ok(());
    }
    let temporary =
        path.with_extension(crate::workloads::TaskIdentity::local("helper", None).attempt_id);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| crate::Error::Io(error.to_string()))?;
    file.write_all(source.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&temporary, path))
        .map_err(|error| crate::Error::Io(error.to_string()))
}

pub(crate) fn publish_support(root: &std::path::Path) -> Result<(), crate::Error> {
    for (name, source) in [
        (
            "asb_material_direct.py",
            include_str!("../../assets/benchmarks/asb_material_direct.py"),
        ),
        (
            "asb_material_shared_network.py",
            include_str!("../../assets/benchmarks/asb_material_shared_network.py"),
        ),
        (
            "asb_material_network_runtime.py",
            include_str!("../../assets/benchmarks/asb_material_network_runtime.py"),
        ),
        (
            "asb_material_network_identity.py",
            include_str!("../../assets/benchmarks/asb_material_network_identity.py"),
        ),
        (
            "asb_material_networks.py",
            include_str!("../../assets/benchmarks/asb_material_networks.py"),
        ),
        (
            "asb_terminal_session.py",
            include_str!("../../assets/benchmarks/asb_terminal_session.py"),
        ),
        (
            "asb_material_environment.py",
            include_str!("../../assets/benchmarks/asb_material_environment.py"),
        ),
        (
            "asb_material_runtime.py",
            include_str!("../../assets/benchmarks/asb_material_runtime.py"),
        ),
        (
            "asb_material_services.py",
            include_str!("../../assets/benchmarks/asb_material_services.py"),
        ),
        (
            "asb_material_declarations.py",
            include_str!("../../assets/benchmarks/asb_material_declarations.py"),
        ),
        (
            "asb_material_mounts.py",
            include_str!("../../assets/benchmarks/asb_material_mounts.py"),
        ),
        (
            "asb_material_bind_graph.py",
            include_str!("../../assets/benchmarks/asb_material_bind_graph.py"),
        ),
        (
            "asb_material_binds.py",
            include_str!("../../assets/benchmarks/asb_material_binds.py"),
        ),
        (
            "asb_material_exec.py",
            include_str!("../../assets/benchmarks/asb_material_exec.py"),
        ),
        (
            "asb_material_root.py",
            include_str!("../../assets/benchmarks/asb_material_root.py"),
        ),
        (
            "asb_material.py",
            include_str!("../../assets/benchmarks/asb_material.py"),
        ),
        (
            "asb_harness.py",
            include_str!("../../assets/benchmarks/asb_harness.py"),
        ),
        (
            "asb_retained.py",
            include_str!("../../assets/benchmarks/asb_retained.py"),
        ),
        (
            "asb_resources.py",
            include_str!("../../assets/benchmarks/asb_resources.py"),
        ),
        (
            "asb_docker.py",
            include_str!("../../assets/benchmarks/asb_docker.py"),
        ),
        (
            "asb_execution.py",
            include_str!("../../assets/benchmarks/asb_execution.py"),
        ),
        (
            "asb_task_server.py",
            include_str!("../../assets/benchmarks/asb_task_server.py"),
        ),
        (
            "asb_guest.py",
            include_str!("../../assets/benchmarks/asb_guest.py"),
        ),
        (
            "asb_snapshot_guest.py",
            include_str!("../../assets/benchmarks/asb_snapshot_guest.py"),
        ),
        (
            "asb_snapshot_control.py",
            include_str!("../../assets/benchmarks/asb_snapshot_control.py"),
        ),
        (
            "asb_snapshot_service.py",
            include_str!("../../assets/benchmarks/asb_snapshot_service.py"),
        ),
        (
            "asb_snapshot_send.py",
            include_str!("../../assets/benchmarks/asb_snapshot_send.py"),
        ),
        (
            "asb_snapshot_handoff.py",
            include_str!("../../assets/benchmarks/asb_snapshot_handoff.py"),
        ),
        (
            "asb_snapshot_workspace.py",
            include_str!("../../assets/benchmarks/asb_snapshot_workspace.py"),
        ),
        (
            "asb_snapshot_topology.py",
            include_str!("../../assets/benchmarks/asb_snapshot_topology.py"),
        ),
        (
            "asb_runtime.py",
            include_str!("../../assets/benchmarks/asb_runtime.py"),
        ),
        (
            "asb_staging.py",
            include_str!("../../assets/benchmarks/asb_staging.py"),
        ),
        (
            "asb_relay.py",
            include_str!("../../assets/benchmarks/asb_relay.py"),
        ),
    ] {
        publish_asset(&root.join(name), source)?;
    }
    Ok(())
}

pub(crate) fn decode_base64(value: &Value) -> Result<Vec<u8>, String> {
    let bytes = value
        .as_str()
        .ok_or("controller output is not encoded bytes")?
        .as_bytes();
    if bytes.len() % 4 != 0 || bytes.len() as u64 > MAX_MESSAGE {
        return Err("invalid bounded output encoding".into());
    }
    let digit = |byte| match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err("invalid output encoding".to_owned()),
    };
    let mut result = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let a = digit(chunk[0])?;
        let b = digit(chunk[1])?;
        let c = if chunk[2] == b'=' {
            0
        } else {
            digit(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            digit(chunk[3])?
        };
        if (chunk[2] == b'=' && (chunk[3] != b'=' || b & 15 != 0))
            || (chunk[3] == b'=' && chunk[2] != b'=' && c & 3 != 0)
            || ((chunk[2] == b'=' || chunk[3] == b'=') && (index + 1) * 4 != bytes.len())
        {
            return Err("invalid padded output encoding".into());
        }
        result.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            result.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            result.push((c << 6) | d);
        }
    }
    Ok(result)
}

#[cfg(unix)]
#[path = "controller/unix.rs"]
mod implementation;
#[cfg(not(unix))]
#[path = "controller/legacy.rs"]
mod implementation;
pub(crate) use implementation::Controller;
#[cfg(all(test, target_os = "linux"))]
#[path = "../../tests/unit/material_transport_custody.rs"]
mod material_custody_tests;
#[cfg(test)]
#[path = "../../tests/unit/controller_transport.rs"]
mod transport_tests;

#[cfg(test)]
mod tests {
    fn completed_grade() -> super::CompletedGrade {
        let identity = crate::workloads::TaskIdentity::local("fixture", Some("task"));
        super::CompletedGrade::decode(
            &serde_json::json!({"event":"evaluated", "result":{
                "workload":"fixture", "task_id":"task", "dataset":null,
                "outcome":"unresolved", "resolved":false
            }}),
            &identity,
        )
        .unwrap()
    }
    #[test]
    fn published_harbor_assets_import_without_the_terminal_bench_1_adapter() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("harbor-assets", None).attempt_id);
        std::fs::create_dir(&root).unwrap();
        super::publish_support(&root).unwrap();
        for (name, source) in [
            (
                "asb_harbor.py",
                include_str!("../../assets/benchmarks/asb_harbor.py"),
            ),
            (
                "asb_terminal_catalog.py",
                include_str!("../../assets/benchmarks/asb_terminal_catalog.py"),
            ),
        ] {
            super::publish_asset(&root.join(name), source).unwrap();
        }
        assert_eq!(
            std::fs::read_to_string(root.join("asb_harness.py")).unwrap(),
            include_str!("../../assets/benchmarks/asb_harness.py")
        );
        assert!(!root.join("asb_terminal_bench_1.py").exists());
        let mut command = std::process::Command::new("python3");
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default());
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        let output = command.args(["-I", "-B", "-c", "import sys; sys.path.insert(0, sys.argv[1]); import asb_harbor, asb_harness; assert asb_harbor.record_grading_start is asb_harness.record_grading_start; assert 'asb_terminal_bench_1' not in sys.modules"])
            .arg(&root)
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn helper_publication_is_private_idempotent_and_does_not_follow_destination_symlinks() {
        use std::os::unix::fs::MetadataExt;
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("helper-publication", None).attempt_id);
        std::fs::create_dir(&root).unwrap();
        let file = root.join("helper.py");
        super::publish_asset(&file, "first").unwrap();
        let before = std::fs::metadata(&file).unwrap().ino();
        super::publish_asset(&file, "first").unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().ino(), before);
        super::publish_asset(&file, "second").unwrap();
        assert_ne!(std::fs::metadata(&file).unwrap().ino(), before);
        let other = root.join("other");
        std::fs::write(&other, "untouched").unwrap();
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&other, &file).unwrap();
        super::publish_asset(&file, "second").unwrap();
        assert_eq!(std::fs::read_to_string(other).unwrap(), "untouched");
        assert!(!file.is_symlink());
        std::fs::remove_dir_all(root).unwrap();
    }
    use super::*;
    use serde_json::json;

    #[test]
    fn explicit_cleanup_waits_for_confirmation_and_exit_beyond_drop_grace() {
        let mut command = ControllerCommand::new("python3").unwrap();
        command.args(["-u", "-c", "import json,sys,time\nprint(json.dumps(dict(event='ready')),flush=True)\nrequest=json.loads(sys.stdin.readline())\nassert request['op']=='close'\ntime.sleep(3.2)\nprint(json.dumps(dict(event='closed',cleanup='removed')),flush=True)\n"]);
        let mut controller = Controller::spawn(&mut command).unwrap();
        controller
            .receive(Instant::now() + Duration::from_secs(3))
            .unwrap();
        controller
            .finish(completed_grade(), Duration::from_secs(8))
            .unwrap();
        assert!(controller.child().try_wait().unwrap().unwrap().success());
    }

    #[test]
    fn cleanup_failure_missing_ack_and_failed_controller_exit_are_not_success() {
        for (event, cleanup, code) in [
            ("closed", "cleanup_failed", 0),
            ("closed", "not_created", 0),
            ("completed", "removed", 0),
            ("closed", "removed", 1),
        ] {
            let script = format!(
                "import json,sys\nprint(json.dumps(dict(event='ready')),flush=True)\nsys.stdin.readline()\nprint(json.dumps(dict(event='{event}',cleanup='{cleanup}')),flush=True)\nsys.exit({code})\n"
            );
            let mut command = ControllerCommand::new("python3").unwrap();
            command.args(["-u", "-c", &script]);
            let mut controller = Controller::spawn(&mut command).unwrap();
            controller
                .receive(Instant::now() + Duration::from_secs(3))
                .unwrap();
            let result = controller.finish(completed_grade(), Duration::from_secs(3));
            if matches!(cleanup, "not_created" | "removed") && event == "closed" {
                let settled = result.unwrap();
                assert!(settled.diagnostic.is_some());
            } else {
                assert!(result.is_err());
            }
        }
    }

    #[test]
    fn cancellation_drains_inflight_events_and_waits_for_owned_cleanup() {
        let mut command = ControllerCommand::new("python3").unwrap();
        command.args(["-u", "-c", "import json,sys,time\nprint(json.dumps(dict(event='ready')),flush=True)\nassert sys.stdin.read()==''\nfor event in ['output','execution_ended','completed','error','future',None]:\n print(json.dumps(dict(event=event,private='never print this')),flush=True)\ntime.sleep(3.2)\nprint(json.dumps(dict(event='closed',cleanup='removed')),flush=True)\nsys.exit(1)\n"]);
        let mut controller = Controller::spawn(&mut command).unwrap();
        controller
            .receive(Instant::now() + Duration::from_secs(3))
            .unwrap();
        controller.cancel(Duration::from_secs(8)).unwrap();
        assert!(!controller.child().try_wait().unwrap().unwrap().success());
    }

    #[test]
    fn cancelled_startup_without_a_created_resource_can_be_confirmed() {
        let mut command = ControllerCommand::new("python3").unwrap();
        command.args(["-u", "-c", "import json,sys\nprint(json.dumps(dict(event='error')),flush=True)\nassert sys.stdin.read()==''\nprint(json.dumps(dict(event='closed',cleanup='not_created')),flush=True)\nsys.exit(1)\n"]);
        let mut controller = Controller::spawn(&mut command).unwrap();
        assert!(
            controller
                .receive(Instant::now() + Duration::from_secs(3))
                .is_err()
        );
        controller.cancel(Duration::from_secs(3)).unwrap();
    }

    #[test]
    fn cancellation_never_confuses_child_exit_with_resource_removal() {
        for cleanup in [
            None,
            Some("pending"),
            Some("ownership_mismatch"),
            Some("error"),
        ] {
            let ack = cleanup.map_or(String::new(), |cleanup| {
                format!("print(json.dumps(dict(event='closed',cleanup='{cleanup}')),flush=True)\n")
            });
            let script = format!(
                "import json,sys\nprint(json.dumps(dict(event='ready')),flush=True)\nassert sys.stdin.read()==''\n{ack}"
            );
            let mut command = ControllerCommand::new("python3").unwrap();
            command.args(["-u", "-c", &script]);
            let mut controller = Controller::spawn(&mut command).unwrap();
            controller
                .receive(Instant::now() + Duration::from_secs(3))
                .unwrap();
            assert!(controller.cancel(Duration::from_secs(3)).is_err());
        }
    }

    #[test]
    fn preserved_interruption_can_finish_cancellation_but_not_normal_completion() {
        for normal in [false, true] {
            let mut command = ControllerCommand::new("python3").unwrap();
            command.args(["-u", "-c", "import json,sys\nprint(json.dumps(dict(event='ready')),flush=True)\nsys.stdin.readline()\nprint(json.dumps(dict(event='closed',cleanup='stopped-preserved')),flush=True)\n"]);
            let mut controller = Controller::spawn(&mut command).unwrap();
            controller
                .receive(Instant::now() + Duration::from_secs(3))
                .unwrap();
            if normal {
                let settled = controller
                    .finish(completed_grade(), Duration::from_secs(3))
                    .unwrap();
                assert_eq!(settled.disposition, ResourceDisposition::StoppedPreserved);
                assert!(settled.diagnostic.is_some());
            } else {
                assert_eq!(
                    controller.cancel(Duration::from_secs(3)).unwrap(),
                    ResourceDisposition::StoppedPreserved
                );
            }
        }
    }

    #[test]
    fn cancellation_drains_until_deadline_not_an_unbounded_event_stream() {
        let mut command = ControllerCommand::new("python3").unwrap();
        command.args(["-u", "-c", "import json,sys,time\nprint(json.dumps(dict(event='ready')),flush=True)\nassert sys.stdin.read()==''\nfor i in range(30):\n print(json.dumps(dict(event='output')),flush=True)\n time.sleep(.01)\n"]);
        let mut controller = Controller::spawn(&mut command).unwrap();
        controller
            .receive(Instant::now() + Duration::from_secs(3))
            .unwrap();
        let started = Instant::now();
        assert!(controller.cancel(Duration::from_millis(50)).is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn acknowledged_resources_do_not_settle_before_controller_reaping() {
        for normal in [false, true] {
            #[cfg(target_os = "linux")]
            let scope = crate::process_cleanup::test_support::Scope::new();
            let mut command = ControllerCommand::new("python3").unwrap();
            command.args(["-u","-c","import json,sys,time\nprint(json.dumps(dict(event='ready')),flush=True)\nsys.stdin.readline()\nprint(json.dumps(dict(event='closed',cleanup='stopped-preserved')),flush=True)\ntime.sleep(10)\n"]);
            let mut controller = Controller::spawn(&mut command).unwrap();
            controller
                .receive(Instant::now() + Duration::from_secs(3))
                .unwrap();
            let failed = if normal {
                controller
                    .finish(completed_grade(), Duration::from_millis(100))
                    .unwrap_err()
            } else {
                controller.cancel(Duration::from_millis(100)).unwrap_err()
            };
            assert!(failed.contains("did not exit"), "{failed}");
            #[cfg(unix)]
            {
                // Expired bounded cleanup has already transferred the child;
                // an acknowledgement alone cannot claim successful settlement.
                assert!(failed.contains("retains custody"), "{failed}");
                drop(controller);
                #[cfg(target_os = "linux")]
                {
                    scope.wait_finished();
                    assert_eq!(scope.count(), 1);
                }
            }
            #[cfg(not(unix))]
            {
                assert!(controller.child().try_wait().unwrap().is_none());
                controller.child().kill().unwrap();
                controller.child().wait().unwrap();
            }
        }
    }

    #[test]
    fn cancellation_reports_each_actual_disposition_only_after_reaping() {
        for (status, expected) in [
            ("removed", ResourceDisposition::Removed),
            ("not_created", ResourceDisposition::NotCreated),
            ("stopped-preserved", ResourceDisposition::StoppedPreserved),
        ] {
            let script = format!(
                "import json,sys\nprint(json.dumps(dict(event='ready')),flush=True)\nassert sys.stdin.read()==''\nprint(json.dumps(dict(event='closed',cleanup='{status}')),flush=True)\nsys.exit(3)\n"
            );
            let mut command = ControllerCommand::new("python3").unwrap();
            command.args(["-u", "-c", &script]);
            let mut controller = Controller::spawn(&mut command).unwrap();
            controller
                .receive(Instant::now() + Duration::from_secs(3))
                .unwrap();
            assert_eq!(controller.cancel(Duration::from_secs(3)).unwrap(), expected);
            assert_eq!(
                controller.child().try_wait().unwrap().unwrap().code(),
                Some(3)
            );
        }
    }

    #[test]
    fn output_encoding_preserves_binary_and_rejects_noncanonical_padding() {
        for (encoded, expected) in [
            ("", &b""[..]),
            ("YQ==", &b"a"[..]),
            ("YWI=", &b"ab"[..]),
            ("YWJj", &b"abc"[..]),
            ("AP+A", &[0, 255, 128][..]),
        ] {
            assert_eq!(decode_base64(&json!(encoded)).unwrap(), expected);
        }
        for invalid in [
            "a", "====", "YQ=x", "YQ==YQ==", "YR==", "YWJ=", "Y WJ", "!!!!",
        ] {
            assert!(decode_base64(&json!(invalid)).is_err(), "{invalid}");
        }
    }

    #[test]
    fn private_requests_round_trip_and_eof_reaps_the_owned_controller() {
        let mut command = ControllerCommand::new("python3").unwrap();
        command.args(["-u", "-c", "import json,sys\nprint(json.dumps({'event':'ready'}),flush=True)\nfor line in sys.stdin:\n request=json.loads(line)\n print(json.dumps({'event':'completed','value':request['value']}),flush=True)\n"]);
        let mut controller = Controller::spawn(&mut command).unwrap();
        assert_eq!(
            controller
                .receive(Instant::now() + Duration::from_secs(5))
                .unwrap()["event"],
            "ready"
        );
        let response = controller
            .request(
                json!({"value":"private fixture"}),
                EventKind::Completed,
                Duration::from_secs(5),
            )
            .unwrap();
        assert_eq!(response["value"], "private fixture");
        let pid = controller.child().id();
        drop(controller);
        #[cfg(target_os = "linux")]
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn malformed_or_oversized_messages_do_not_echo_private_payloads() {
        for body in [
            "print('private fixture',flush=True)",
            "print('x'*1048577,flush=True)",
        ] {
            let mut command = ControllerCommand::new("python3").unwrap();
            command.args(["-u", "-c", body]);
            let mut controller = Controller::spawn(&mut command).unwrap();
            let error = controller
                .receive(Instant::now() + Duration::from_secs(5))
                .unwrap_err();
            assert!(!error.contains("private fixture"));
            assert!(error.contains("JSON") || error.contains("framing"));
        }
    }

    #[test]
    fn typed_error_events_keep_local_diagnostics_but_never_expose_sdk_bodies() {
        for (class, reason, expected) in [
            (
                "ExecutionError",
                "fixed local reason".to_owned(),
                Some("workload execution failed: fixed local reason"),
            ),
            ("SdkError", "private SDK response".to_owned(), None),
            ("ExecutionError", "private\nSDK response".to_owned(), None),
            ("ExecutionError", "private\rSDK response".to_owned(), None),
            ("ExecutionError", "private".repeat(100), None),
        ] {
            let mut command = ControllerCommand::new("python3").unwrap();
            command
                .args([
                    "-I",
                    "-S",
                    "-B",
                    "-u",
                    "-c",
                    "import json,sys;print(sys.argv[1],flush=True);sys.stdin.read()",
                ])
                .arg(json!({"event":"error","error_class":class,"reason":reason}).to_string());
            let mut controller = Controller::spawn(&mut command).unwrap();
            let error = controller
                .receive(Instant::now() + Duration::from_secs(3))
                .unwrap_err();
            match expected {
                Some(expected) => assert_eq!(error, expected),
                None => {
                    assert_eq!(
                        error,
                        "workload controller could not complete the operation; inspect the private task artifacts"
                    );
                    assert!(!error.contains("private SDK"));
                }
            }
            controller
                .complete_response(Instant::now() + Duration::from_secs(3))
                .unwrap();
        }
    }

    #[test]
    fn generic_transport_still_accepts_untagged_one_shot_inspection_responses() {
        let mut command = ControllerCommand::new("python3").unwrap();
        command.args(["-I", "-S", "-B", "-u", "-c",
            "import json,sys;json.loads(sys.stdin.readline());print(json.dumps(dict(ok=True,scope='inspection')),flush=True);sys.stdin.read()"]);
        let mut controller = Controller::spawn(&mut command).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        controller
            .send(&json!({"reference":"fixture"}), deadline)
            .unwrap();
        assert_eq!(
            controller.receive(deadline).unwrap(),
            json!({"ok":true,"scope":"inspection"})
        );
        controller.complete_response(deadline).unwrap();
    }
}
