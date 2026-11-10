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

#[cfg(unix)]
#[test]
fn cassette_browser_wrap_and_scroll_keys_work_in_narrow_terminal() {
    let fixture = Fixture::new();
    let payload =
        serde_json::json!({"stderr":"failure", "long":"x".repeat(500)}).to_string() + "\n";
    let path = fixture.write("recording.jsonl", &payload);
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/cassette_tui.py"
        ))
        .arg(env!("CARGO_BIN_EXE_asb-replay-debug"))
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), payload);
}

#[cfg(unix)]
#[test]
fn terminal_manager_creates_edits_and_enables_rules_from_evidence() {
    use std::os::unix::fs::PermissionsExt;
    for mode in ["mismatch", "cassette"] {
        let fixture = Fixture::new();
        fixture.write(".asb/project.toml", "# retained project comment\n");
        fixture.write("case.mismatch.json", r#"{"schema_version":1,"mismatch":{"sequence":1,"path":"$.body.input[0].output","category":"json_value"},"expected":{"method":"POST","url":"/responses","body":{"input":[{"output":"seed=1"}]}},"actual":{"method":"POST","url":"/responses","body":{"input":[{"output":"seed=2"}]}}}"#);
        fixture.write("recording.jsonl", "{\"request\":{\"method\":\"POST\",\"body\":\"{\\\"messages\\\":[{\\\"role\\\":\\\"tool\\\",\\\"content\\\":\\\"seed=1\\\"}]}\"},\"response\":{}}\n");
        let editor = fixture.write(
            "editor",
            "#!/bin/sh\nprintf '\\n// edited by test\\n' >> \"$1\"\n",
        );
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/normalizer_tui.py"
            ))
            .arg(env!("CARGO_BIN_EXE_asb-replay-debug"))
            .arg(&fixture.path)
            .arg(mode)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let config = std::fs::read_to_string(fixture.path.join(".asb/project.toml")).unwrap();
        assert!(config.contains("# retained project comment"));
        assert!(config.contains("enabled = true"));
        assert!(config.contains("workloads = [\"swe-bench\"]"));
        let module =
            std::fs::read_to_string(fixture.path.join("normalizers/debugger-suggestion-1.mjs"))
                .unwrap();
        assert!(module.contains("seed=1"));
        if mode == "mismatch" {
            assert!(module.contains("seed=2"));
        }
        assert!(module.contains("// edited by test"));
        let module_path = fixture.path.join("normalizers/debugger-suggestion-1.mjs");
        let script = "import {pathToFileURL} from 'node:url'; const m=await import(pathToFileURL(process.argv[1])); if(m.normalizeToolResult({text:'seed=1'}) !== 'seed=1' || m.normalizeToolResult({text:'unrelated'}) !== 'unrelated' || (process.argv[2] === 'mismatch' && m.normalizeToolResult({text:'seed=2'}) !== 'seed=1')) process.exit(1);";
        assert!(
            std::process::Command::new("node")
                .args(["--input-type=module", "-e", script])
                .arg(module_path)
                .arg(mode)
                .status()
                .unwrap()
                .success()
        );
    }
}
