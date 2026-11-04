//! Agent-only stopped principal launch tests.
#![cfg(target_os = "linux")]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn stopped_principal_execs_in_place_after_external_resume() {
    let root = std::env::temp_dir().join(format!("asb-principal-gate-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let output = root.join("pid");
    let mut child = Owned(
        Command::new(env!("CARGO_BIN_EXE_asb"))
            .args([
                "__asb-measure-launch",
                "--asb-launch-v1",
                "0",
                "/bin/sh",
                "-c",
                "printf %s $$ > \"$ASB_TEST_PID_FILE\"",
            ])
            .env("ASB_TEST_PID_FILE", &output)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let pid = child.0.id();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "launcher exited before stopping"
        );
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        if status
            .lines()
            .any(|line| line.starts_with("State:") && line.split_whitespace().nth(1) == Some("T"))
        {
            break;
        }
        assert!(Instant::now() < deadline, "launcher did not stop");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!output.exists(), "agent executed before attachment");
    assert!(
        Command::new("/bin/kill")
            .args(["-CONT", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let status = child.0.wait().unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&output).unwrap(), pid.to_string());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_principal_frame_is_rejected() {
    let status = Command::new(env!("CARGO_BIN_EXE_asb"))
        .args(["__asb-measure-launch", "malformed"])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2));
}
