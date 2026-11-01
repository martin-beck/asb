//! The private bridge entry is selected by its owning registered adapter.
use std::io::Write;
use std::process::{Command, Stdio};

fn record(args: &[&str], sidecar: &std::path::Path) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_asb"))
        .args(args)
        .args(["--mode", "record", "--sidecar"])
        .arg(sidecar)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let frame = serde_json::json!({"kind":"record_terminal","schema_version":1,
        "action":{"kind":"terminal_action","schema_version":1,"command":"echo test","is_input":false,"timeout":null,"reset":false},
        "observation":{"kind":"terminal_observation","schema_version":1,"content":"test\n","is_error":false,"command":"echo test","exit_code":0,"timeout":false,"metadata":{"exit_code":0}}});
    writeln!(child.stdin.take().unwrap(), "{frame}").unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn private_bridge_dispatches_only_registered_owners() {
    for args in [
        vec!["__asb-agent-bridge"],
        vec!["__asb-agent-bridge", "not-an-agent"],
        vec!["__asb-agent-bridge", "pi"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_asb"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
    let root = std::env::temp_dir().join(format!(
        "asb-bridge-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let sidecar = root.join("sidecar.jsonl");
    let output = record(&["__asb-agent-bridge", "openhands"], &sidecar);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let transcript = std::fs::read_to_string(&sidecar).unwrap();
    assert!(transcript.contains("\"event\":\"launch\""));
    assert!(transcript.contains("\"event\":\"write\""));
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"kind\":\"ack\""));
    assert!(transcript.contains("\"event\":\"exit\""));
    let legacy_sidecar = root.join("legacy-sidecar.jsonl");
    let legacy = record(&["__asb-openhands-native-bridge"], &legacy_sidecar);
    assert!(
        legacy.status.success(),
        "{}",
        String::from_utf8_lossy(&legacy.stderr)
    );
    assert_eq!(
        std::fs::read(&legacy_sidecar).unwrap(),
        std::fs::read(&sidecar).unwrap()
    );
    std::fs::remove_dir_all(root).unwrap();
}
