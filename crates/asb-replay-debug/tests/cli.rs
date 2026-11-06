//! Full binary checks for offline rendering, errors, and source preservation.

#[path = "../src/test_support.rs"]
mod test_support;

use std::process::Command;
use test_support::Fixture;

#[test]
fn prints_a_standalone_mismatch_without_initialization_or_modifying_input() {
    let fixture = Fixture::new();
    let original = r#"{"schema_version":1,"mismatch":{"sequence":18,"category":"json_value","path":"$.input[0].output"},"expected":{"method":"POST","url":"/responses","body":{"input":[{"output":"Ran 6 tests in 0.001s\n\nOK"}]}},"actual":{"method":"POST","url":"/responses","body":{"input":[{"output":"Ran 6 tests in 0.002s\n\nOK"}]}}}"#;
    let path = fixture.write("example.mismatch.json", original);
    let output = Command::new(env!("CARGO_BIN_EXE_asb-replay-debug"))
        .args(["--print", path.to_str().unwrap()])
        .current_dir(&fixture.path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("1 changed"));
    assert!(text.contains("execution: unknown"));
    assert!(text.contains("-    1 Ran 6 tests in 0.001s"));
    assert!(text.contains("+    1 Ran 6 tests in 0.002s"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert_eq!(std::fs::read_dir(&fixture.path).unwrap().count(), 1);
}

#[test]
fn reports_invalid_input_and_noninteractive_mode_without_terminal_escapes() {
    let fixture = Fixture::new();
    let path = fixture.write("broken.mismatch.json", "{broken");
    let output = Command::new(env!("CARGO_BIN_EXE_asb-replay-debug"))
        .arg(&path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!output.stderr.contains(&0x1b));
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid mismatch JSON"));
    let valid=fixture.write("valid.mismatch.json",r#"{"schema_version":1,"mismatch":{},"expected":{"method":"POST","url":"/responses","body":null},"actual":{"method":"POST","url":"/responses","body":null}}"#);
    let output = Command::new(env!("CARGO_BIN_EXE_asb-replay-debug"))
        .arg(valid)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("use --print"));
    assert!(output.stdout.is_empty());
}

#[test]
fn prints_selected_legacy_cassette_without_modifying_it() {
    let fixture = Fixture::new();
    let original = "{\"request\":{\"method\":\"POST\"},\"response\":\"fixture\"}\n";
    let path = fixture.write("recording.jsonl", original);
    let output = Command::new(env!("CARGO_BIN_EXE_asb-replay-debug"))
        .args([
            "--cassette",
            path.to_str().unwrap(),
            "--track",
            "1",
            "--print",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("legacy (completion unknown)"));
    assert!(text.contains("fixture"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}
