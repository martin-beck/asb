//! Hermetic lifecycle failures and local protocol fixtures, no live upstream.
use super::*;
use std::fs;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "asb-tape-boundaries-{}",
            crate::workloads::TaskIdentity::local("boundary", None).attempt_id
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = crate::fixture_writer::cleanup(&self.0);
    }
}

#[test]
fn record_uses_one_filesystem_resolved_destination_for_producer_and_receipt() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join("real/deep")).unwrap();
    std::os::unix::fs::symlink(fixture.0.join("real/deep"), fixture.0.join("link")).unwrap();
    for (requested, expected) in [
        (
            fixture.0.join("real/deep/../dot.jsonl"),
            fixture.0.join("real/dot.jsonl"),
        ),
        (
            fixture.0.join("link/../symlink-parent.jsonl"),
            fixture.0.join("real/symlink-parent.jsonl"),
        ),
    ] {
        let settings = Settings {
            mode: Mode::Record,
            cassette: requested,
            respect_record_timings: false,
            workspace: fixture.0.clone(),
            invocation_dir: fixture.0.clone(),
            wire_extension: None,
        };
        let mut session = ADAPTER
            .start(Some("http://127.0.0.1:1/v1"), Some(&settings))
            .unwrap();
        session.finish().unwrap();
        let evidence = session.capture_evidence().unwrap();
        assert_eq!(evidence.output, expected);
        assert!(evidence.complete && evidence.bytes == 0);
        assert!(expected.is_file());
        drop(session);
    }
    let relative = PathBuf::from("target/capture-relative-destinations")
        .join(crate::tools::runtime::unique_id());
    let requested = relative.join("child/../trace.jsonl");
    let expected = std::env::current_dir()
        .unwrap()
        .join(&relative)
        .join("trace.jsonl");
    assert_eq!(recording_destination(&requested).unwrap(), expected);
    fs::remove_dir_all(relative).unwrap();
}

#[test]
fn external_override_never_receives_private_capture_capability_or_invents_evidence() {
    const CHILD: &str = "ASB_CAPTURE_OVERRIDE_TEST";
    if std::env::var_os(CHILD).is_some() {
        let fixture = Fixture::new();
        let settings = Settings {
            mode: Mode::Record,
            cassette: fixture.0.join("trace.jsonl"),
            respect_record_timings: false,
            workspace: fixture.0.clone(),
            invocation_dir: fixture.0.clone(),
            wire_extension: None,
        };
        let mut session = ADAPTER
            .start(Some("http://127.0.0.1:1/v1"), Some(&settings))
            .unwrap();
        session.finish().unwrap();
        assert!(session.capture_evidence().is_none());
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let script = fixture.0.join("override");
    let marker = fixture.0.join("clean-env");
    let proxy = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/scripts/llmtape.mjs");
    crate::write_test_file(&fixture.0, &script, format!("#!/bin/sh\nif [ \"${{ASB_TAPE_CAPTURE_NONCE+x}}\" = x ] || [ \"${{ASB_TAPE_CAPTURE_RECEIPT+x}}\" = x ]; then exit 67; fi\nprintf clean > {}\nexec node {} \"$@\"\n", shell_quote(&marker), shell_quote(&proxy))).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap());
    child.args(["--exact", "proxy::adapters::llmtape::boundary_tests::external_override_never_receives_private_capture_capability_or_invents_evidence", "--nocapture"])
        .env_clear().env(CHILD, "1").env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &fixture.0).env("ASB_LLM_TAPE", &script)
        .env("ASB_TAPE_CAPTURE_NONCE", "enclosing-private-invocation")
        .env("ASB_TAPE_CAPTURE_RECEIPT", fixture.0.join("must-not-write"))
        .current_dir(&fixture.0);
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        child.env("LLVM_PROFILE_FILE", profile);
    }
    let output = child.output().unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(marker).unwrap(), "clean");
    assert!(!fixture.0.join("must-not-write").exists());
}

#[test]
fn capture_evidence_and_retained_diagnostics_survive_independent_cleanup_failures() {
    use sha2::{Digest, Sha256};
    for valid_receipt in [false, true] {
        let fixture = Fixture::new();
        let output = fixture.0.join("trace.jsonl");
        fs::write(&output, b"{}\n").unwrap();
        let scratch = Scratch::create(&fixture.0).unwrap();
        let retained = scratch.path.clone();
        let receipt = scratch.path.join("capture.json");
        let mut child = Command::new("/bin/sh")
            .args(["-c", if valid_receipt { "exit 2" } else { "exit 0" }])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        fs::write(
            &receipt,
            serde_json::to_vec(&serde_json::json!({
                "schema_version":1,"mode":"record","pid":pid,
                "nonce":if valid_receipt {"expected"} else {"wrong-invocation"},
                "output":output,"accepted":1,"completed":1,"failed":0,"inflight":0,
                "recorded":1,"inference_requests":1,"bytes":3,
                "sha256":format!("{:x}",Sha256::digest(b"{}\n")),"complete":true,
            }))
            .unwrap(),
        )
        .unwrap();
        let mut session = RunningLlmtape {
            child: Some(child),
            builtin: true,
            url: String::new(),
            progress: None,
            mismatch_evidence: None,
            cassette: output.clone(),
            mode: Mode::Record,
            cleanup_confirmed: false,
            capture_request: Some(crate::proxy::capture::Request {
                receipt: receipt.clone(),
                output,
                nonce: "expected".into(),
                pid,
            }),
            capture: None,
            _scratch: scratch,
        };
        let error = session.finish().unwrap_err().to_string();
        assert!(error.contains("retained recorder state"));
        assert_eq!(session.cleanup_confirmed(), !valid_receipt);
        assert_eq!(
            session
                .capture_evidence()
                .is_some_and(|capture| capture.complete),
            valid_receipt
        );
        drop(session);
        assert!(
            retained.is_dir() && receipt.is_file(),
            "uncertain cleanup or invalid capture must retain private evidence"
        );
    }
}

#[test]
fn sealed_incomplete_capture_retains_only_its_private_receipt_scratch() {
    use sha2::{Digest, Sha256};
    let fixture = Fixture::new();
    let output = fixture.0.join("capture.jsonl");
    std::fs::write(&output, b"{}\n").unwrap();
    let payload = std::fs::read(&output).unwrap();
    let scratch = Scratch::create(&fixture.0).unwrap();
    let retained = scratch.path.clone();
    let receipt = scratch.path.join("capture.json");
    std::fs::write(
        &receipt,
        serde_json::json!({
            "schema_version":1,"mode":"record","pid":123,"nonce":"private-invocation",
            "output":output,"accepted":1,"completed":0,"failed":1,"inflight":0,
            "recorded":1,"inference_requests":1,"bytes":payload.len(),
            "sha256":format!("{:x}",Sha256::digest(&payload)),"complete":false,
            "delivery_failures":[{"sequence":1,"method":"POST","path":"/v1/messages","cause":"close"}]
        })
        .to_string(),
    )
    .unwrap();
    let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
    child.wait().unwrap();
    let mut session = RunningLlmtape {
        child: Some(child),
        builtin: true,
        url: String::new(),
        progress: None,
        mismatch_evidence: None,
        cassette: output.clone(),
        mode: crate::tape::Mode::Record,
        cleanup_confirmed: false,
        capture_request: Some(crate::proxy::capture::Request {
            receipt,
            output,
            nonce: "private-invocation".into(),
            pid: 123,
        }),
        capture: None,
        _scratch: scratch,
    };
    assert!(session.finish().is_ok());
    assert!(
        session
            .capture_evidence()
            .is_some_and(|capture| !capture.complete)
    );
    drop(session);
    assert!(retained.is_dir());
}

#[test]
fn existing_recording_destinations_are_refused_before_starting_a_proxy() {
    let fixture = Fixture::new();
    for kind in ["file", "directory", "dangling-link"] {
        let path = fixture.0.join(kind);
        match kind {
            "file" => fs::write(&path, "preserve recorded bytes").unwrap(),
            "directory" => fs::create_dir(&path).unwrap(),
            _ => std::os::unix::fs::symlink(fixture.0.join("absent"), &path).unwrap(),
        }
        let settings = Settings {
            mode: Mode::Record,
            cassette: path,
            respect_record_timings: false,
            workspace: fixture.0.clone(),
            invocation_dir: fixture.0.clone(),
            wire_extension: None,
        };
        let error = ADAPTER
            .start(Some("http://127.0.0.1:1/v1"), Some(&settings))
            .err()
            .unwrap();
        assert!(error.to_string().contains("already exists"));
        assert!(!fs::read_dir(&fixture.0).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".asb-tape-")
        }));
    }
    assert_eq!(
        fs::read_to_string(fixture.0.join("file")).unwrap(),
        "preserve recorded bytes"
    );
}

#[test]
fn startup_errors_are_specific_and_remove_only_their_private_scratch() {
    const CHILD: &str = "ASB_TEST_TAPE_BOUNDARY";
    if let Ok(scenario) = std::env::var(CHILD) {
        let root = Fixture::new();
        let mut settings = Settings {
            mode: Mode::Record,
            cassette: root.0.join("tape.jsonl"),
            respect_record_timings: false,
            workspace: root.0.join("workspace"),
            invocation_dir: root.0.clone(),
            wire_extension: None,
        };
        let error = match scenario.as_str() {
            "missing-upstream" => ADAPTER.start(None, Some(&settings)).err().unwrap(),
            "missing-cassette" => {
                settings.mode = Mode::Replay;
                ADAPTER.start(None, Some(&settings)).err().unwrap()
            }
            "blocked-parent" => {
                fs::write(root.0.join("blocked"), "file").unwrap();
                settings.cassette = root.0.join("blocked/tape.jsonl");
                ADAPTER.start(None, Some(&settings)).err().unwrap()
            }
            "missing-node" => ADAPTER
                .start(Some("http://127.0.0.1:1/v1"), Some(&settings))
                .err()
                .unwrap(),
            "override" => {
                assert!(matches!(
                    resolve_executable(&root.0, &root.0).unwrap(),
                    ProxyExecutable::Override(_)
                ));
                ADAPTER
                    .start(Some("http://127.0.0.1:1/v1"), Some(&settings))
                    .err()
                    .unwrap()
            }
            _ => panic!("unknown private fixture"),
        }
        .to_string();
        let expected = match scenario.as_str() {
            "missing-upstream" => "record requires a provider API endpoint",
            "missing-cassette" => "cannot copy replay cassette",
            "blocked-parent" => "cannot create cassette directory",
            "missing-node" => "Install Node.js",
            "override" => "Check the ASB_LLM_TAPE executable",
            _ => unreachable!(),
        };
        assert!(error.contains(expected), "{scenario}: {error}");
        assert!(
            !fs::read_dir(&root.0).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".asb-tape-")),
            "failed startup left private state"
        );
        return;
    }
    for scenario in [
        "missing-upstream",
        "missing-cassette",
        "blocked-parent",
        "missing-node",
        "override",
    ] {
        let root = Fixture::new();
        let mut child = Command::new(std::env::current_exe().unwrap());
        child.args(["--exact","proxy::adapters::llmtape::boundary_tests::startup_errors_are_specific_and_remove_only_their_private_scratch","--nocapture"]).env_clear().env(CHILD,scenario).env("HOME",&root.0).env("PATH",&root.0).current_dir(&root.0);
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            child.env("LLVM_PROFILE_FILE", profile);
        }
        if scenario == "override" {
            child.env("ASB_LLM_TAPE", root.0.join("absent-override"));
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{scenario}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn failed_or_nonfile_readiness_is_not_treated_as_pending() {
    let root = Fixture::new();
    fs::create_dir(root.0.join("directory")).unwrap();
    assert!(
        read_ready_port(&root.0.join("directory"), 1)
            .unwrap_err()
            .to_string()
            .contains("not a regular file")
    );
    std::os::unix::fs::symlink("loop", root.0.join("loop")).unwrap();
    assert!(
        read_ready_port(&root.0.join("loop"), 1)
            .unwrap_err()
            .to_string()
            .contains("cannot read tape readiness")
    );
}

#[test]
fn early_proxy_exit_and_legacy_listener_have_distinct_readiness_results() {
    let root = Fixture::new();
    let mut dead = Command::new("/bin/true").spawn().unwrap();
    dead.wait().unwrap();
    let mut dead = StartingChild(Some(dead));
    assert!(
        wait_for_ready(
            &mut dead,
            Some(&root.0.join("absent")),
            0,
            Duration::from_secs(1)
        )
        .unwrap_err()
        .to_string()
        .contains("exited before becoming ready")
    );
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let child = Command::new("/bin/sleep").arg("10").spawn().unwrap();
    let mut child = StartingChild(Some(child));
    assert_eq!(
        wait_for_ready(&mut child, None, port, Duration::from_secs(1)).unwrap(),
        port
    );
    drop(child); // Only this fixture's child is killed and reaped.
}

#[test]
fn replay_finish_reports_absent_and_malformed_progress_then_is_idempotent() {
    let root = Fixture::new();
    for (progress, contents, expected) in [
        (None, None, "progress path is missing"),
        (Some("absent"), None, "did not produce cassette progress"),
        (Some("invalid"), Some("{"), "progress is malformed"),
    ] {
        let mut child = Command::new("/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let progress = progress.map(|name| root.0.join(name));
        if let (Some(path), Some(contents)) = (&progress, contents) {
            fs::write(path, contents).unwrap();
        }
        let mut session = RunningLlmtape {
            child: Some(child),
            builtin: false,
            url: "http://127.0.0.1:1/v1".into(),
            progress,
            mismatch_evidence: None,
            cassette: root.0.join("tape.jsonl"),
            mode: Mode::Replay,
            cleanup_confirmed: false,
            capture_request: None,
            capture: None,
            _scratch: Scratch::create(&root.0).unwrap(),
        };
        assert!(session.finish().unwrap_err().to_string().contains(expected));
        assert!(session.cleanup_confirmed());
        assert!(session.finish().is_ok());
    }
}

#[test]
fn replay_finish_reports_only_validated_structural_mismatch_context() {
    let root = Fixture::new();
    for (contents, expected) in [
        (
            r#"{"consumed":0,"total":1,"rejected":1,"first_mismatch":{"sequence":1,"category":"json_value","path":"$.messages[0].content","string_lengths":{"expected":23,"actual":9},"first_difference_offset":0,"string_features":{"expected":{"lines":1,"has_agents_guidance":false,"has_parent_path":false,"has_workspace_path":false,"has_additional_input":false},"actual":{"lines":3,"has_agents_guidance":true,"has_parent_path":true,"has_workspace_path":false,"has_additional_input":false}}}}"#,
            "first mismatch: request 1, json_value at $.messages[0].content; string lengths 23→9, first differing byte 0; lines 1→3; replay adds AGENTS guidance, parent paths",
        ),
        (
            r#"{"consumed":0,"total":1,"rejected":1,"first_mismatch":{"sequence":1,"category":"secret-value","path":"$.token"}}"#,
            "normalized request or sequence differs",
        ),
    ] {
        let mut child = Command::new("/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let progress = root.0.join(crate::tools::runtime::unique_id());
        fs::write(&progress, contents).unwrap();
        let mut session = RunningLlmtape {
            child: Some(child),
            builtin: false,
            url: "http://127.0.0.1:1/v1".into(),
            progress: Some(progress),
            mismatch_evidence: None,
            cassette: root.0.join("tape.jsonl"),
            mode: Mode::Replay,
            cleanup_confirmed: false,
            capture_request: None,
            capture: None,
            _scratch: Scratch::create(&root.0).unwrap(),
        };
        let error = session.finish().unwrap_err().to_string();
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("secret-value"), "{error}");
    }
}

#[test]
fn materializing_builtin_script_failure_does_not_silently_use_an_incomplete_proxy() {
    // Explicitly isolate environment so a developer override cannot hide this path.
    const CHILD: &str = "ASB_TEST_TAPE_MATERIALIZE";
    if std::env::var_os(CHILD).is_some() {
        let root = Fixture::new();
        fs::create_dir(root.0.join(".asb-tape-proxy.mjs")).unwrap();
        let error = resolve_executable(&root.0, &root.0)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("cannot materialize ASB's built-in tape proxy"));
        return;
    }
    let root = Fixture::new();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact","proxy::adapters::llmtape::boundary_tests::materializing_builtin_script_failure_does_not_silently_use_an_incomplete_proxy"]).env_clear().env(CHILD,"1").env("HOME",&root.0).current_dir(&root.0);
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}
