use super::*;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

#[test]
fn gate_command_preserves_explicit_argument_environment_and_removal_contracts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/gate-fixtures")
        .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
    fs::create_dir_all(&root).unwrap();
    // Never open an executable fixture for writing alongside parallel forks:
    // even CLOEXEC writers can briefly survive until another child's exec.
    let gate = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-process-gate.sh");
    let mut launch = crate::launch::ProcessLaunch::new("fixture-client");
    launch.arg("argument with spaces").arg("literal;$value");
    let (mut command, _) = launch.native(Some(&gate));
    command
        .env("ASB_GATE_FIXTURE", "explicit-value")
        .env_remove("HOME");
    let output = command.output().unwrap();
    assert!(output.status.success(), "{:?}", output.status);
    assert_eq!(output.stdout, b"fixture gate accepted\n");
    let mut launch = crate::launch::ProcessLaunch::new("fixture-client");
    launch.arg("invalid\0argument");
    let (mut invalid, _) = launch.native(Some(&gate));
    assert_eq!(
        invalid.spawn().unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    let (mut missing, _) =
        crate::launch::ProcessLaunch::new("fixture-client").native(Some(&root.join("missing")));
    assert_eq!(missing.spawn().unwrap_err().kind(), io::ErrorKind::NotFound);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn resume_waits_for_owned_child_stop_and_refuses_a_child_that_already_exited() {
    let mut child = Command::new("sh")
        .args(["-c", "kill -STOP $$; exit 7"])
        .spawn()
        .unwrap();
    resume(&mut child).unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(7));
    let mut child = Command::new("sh").args(["-c", "exit 3"]).spawn().unwrap();
    child.wait().unwrap();
    assert!(
        resume(&mut child)
            .unwrap_err()
            .to_string()
            .contains("exited before readiness")
    );
    assert_eq!(cancel_gate(&mut child).unwrap().code(), Some(3));
}

#[test]
fn cancellation_resumes_a_stopped_owned_child_so_term_can_complete() {
    let mut child = Command::new("sh")
        .args(["-c", "kill -STOP $$; exec sleep 10"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap();
        if status
            .lines()
            .any(|line| line.starts_with("State:") && line.split_whitespace().nth(1) == Some("T"))
        {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    let status = cancel_gate(&mut child).unwrap();
    assert!(!status.success());
    assert!(child.try_wait().unwrap().is_some());
}

#[test]
fn launcher_selection_validates_explicit_override_in_a_private_subprocess() {
    const CASE: &str = "ASB_GATE_COVERAGE_CASE";
    if let Ok(case) = std::env::var(CASE) {
        match case.as_str() {
            "default" => assert_eq!(
                executable().unwrap(),
                std::path::PathBuf::from("/proc/self/exe")
            ),
            "invalid" => assert_eq!(
                executable().unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            ),
            "valid" => assert_eq!(executable().unwrap(), std::env::current_exe().unwrap()),
            _ => panic!("invalid fixture case"),
        }
        assert_eq!(run_gate(&[]), std::process::ExitCode::from(2));
        return;
    }
    let name = format!(
        "{}::launcher_selection_validates_explicit_override_in_a_private_subprocess",
        module_path!().split_once("::").unwrap().1
    );
    for (case, path) in [
        ("default", None),
        (
            "invalid",
            Some(std::path::PathBuf::from("relative/missing")),
        ),
        ("valid", Some(std::env::current_exe().unwrap())),
    ] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", &name, "--nocapture"])
            .env(CASE, case)
            .env_remove("ASB_RUNTIME_EXECUTABLE");
        if let Some(path) = path {
            command.env("ASB_RUNTIME_EXECUTABLE", path);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }
}

#[test]
fn root_executable_identity_skips_path_names_and_oversized_files_without_reading_them() {
    assert!(root_executable_identity(OsStr::new("fixture-client")).is_none());
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/gate-fixtures")
        .join(crate::workloads::TaskIdentity::local("identity", None).attempt_id);
    fs::create_dir_all(&root).unwrap();
    let oversized = root.join("oversized-client");
    std::fs::File::create(&oversized)
        .unwrap()
        .set_len(MAX_ROOT_EXECUTABLE_IDENTITY_BYTES + 1)
        .unwrap();
    assert!(root_executable_identity(oversized.as_os_str()).is_none());
    fs::remove_dir_all(root).unwrap();
}
