//! Real CLI repeated sweeps, sharing the owned synthetic run fixtures.
use super::*;

#[path = "repeated_sweep_multi.rs"]
mod multi;

/// Test-owned nonblocking output, with one absolute deadline across all prelude
/// events. A partial line cannot strand a reader thread or bypass the watchdog.
struct Prelude {
    stdout: std::process::ChildStdout,
    buffered: Vec<u8>,
    bytes: usize,
    deadline: std::time::Instant,
}

impl Prelude {
    #[allow(unsafe_code)]
    fn new(child: &mut std::process::Child) -> Self {
        use std::os::fd::AsRawFd;
        let stdout = child.stdout.take().expect("owned fixture stdout");
        let fd = stdout.as_raw_fd();
        // SAFETY: this live owned pipe descriptor is retained by Self; these
        // fcntl calls only query/set its file-status flags and take no pointers.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cannot make fixture stdout nonblocking");
        }
        Self {
            stdout,
            buffered: Vec::new(),
            bytes: 0,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
        }
    }

    fn try_event(&mut self) -> Result<Value, String> {
        use std::io::Read;
        loop {
            if std::time::Instant::now() >= self.deadline {
                return Err("fixture prelude deadline exceeded".into());
            }
            if let Some(end) = self.buffered.iter().position(|byte| *byte == b'\n') {
                if end > 1024 * 1024 || self.bytes > 8 * 1024 * 1024 {
                    return Err("fixture prelude exceeded its byte budget".into());
                }
                let row = serde_json::from_slice(&self.buffered[..end])
                    .map_err(|_| "fixture event is not JSON".to_string())?;
                self.buffered.drain(..=end);
                return Ok(row);
            }
            if self.buffered.len() >= 1024 * 1024 || self.bytes >= 8 * 1024 * 1024 {
                return Err("fixture prelude exceeded its byte budget".into());
            }
            let mut chunk = [0; 4096];
            match self.stdout.read(&mut chunk) {
                Ok(0) => return Err("fixture stdout ended before expected event".into()),
                Ok(count) => {
                    self.bytes += count;
                    self.buffered.extend_from_slice(&chunk[..count]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => (),
                Err(_) => return Err("fixture stdout read failed".into()),
            }
        }
    }

    fn event(&mut self, child: &mut std::process::Child) -> Value {
        self.try_event().unwrap_or_else(|reason| {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{reason}");
        })
    }
}

#[test]
fn fixture_prelude_bounds_partial_lines_without_abandoned_readers() {
    let mut child = Command::new("python3").isolated_env().env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args(["-c", "import sys,time; print('{}',flush=True); sys.stdout.write('partial'); sys.stdout.flush(); time.sleep(30)"])
        .stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut prelude = Prelude::new(&mut child);
    assert_eq!(prelude.event(&mut child), json!({}));
    prelude.deadline = std::time::Instant::now() + std::time::Duration::from_millis(20);
    assert_eq!(
        prelude.try_event().unwrap_err(),
        "fixture prelude deadline exceeded"
    );
    assert!(child.try_wait().unwrap().is_none());
    drop(prelude);
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn fixture_prelude_rejects_invalid_oversized_and_missing_events() {
    for (body, expected) in [
        ("print('invalid',flush=True)", "fixture event is not JSON"),
        (
            "print('x'*(1024*1024+1),flush=True)",
            "fixture prelude exceeded its byte budget",
        ),
        ("pass", "fixture stdout ended before expected event"),
    ] {
        let mut child = Command::new("python3")
            .isolated_env()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .args(["-c", body])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut prelude = Prelude::new(&mut child);
        let result = prelude.try_event();
        drop(prelude);
        let _ = child.kill();
        child.wait().unwrap();
        assert_eq!(result.unwrap_err(), expected);
    }
}

fn sweep(project: &Project, provider: Option<&RecordingProvider>, arguments: &[&str]) -> Output {
    sweep_command(project, provider, arguments)
        .output()
        .unwrap()
}

fn sweep_command(
    project: &Project,
    provider: Option<&RecordingProvider>,
    arguments: &[&str],
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_asb"));
    command.current_dir(&project.0).isolated_env();
    retain_host_rust_toolchain(&mut command);
    command
        .env("HOME", &project.0)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args(["sweep", "sweep.toml", "--json"])
        .args(arguments);
    if let Some(provider) = provider {
        command
            .env("ASB_PROVIDER_BASE_URL", &provider.endpoint)
            .env("OPENROUTER_API_KEY", "fixture-only-key")
            .env("OPENROUTER_BASE_URL", format!("{}/v1", provider.endpoint))
            .env(
                "OPENROUTER_MODELS_URL",
                format!("{}/models", provider.endpoint),
            );
    }
    command
}

fn selections(output: &Output) -> std::collections::BTreeMap<String, Value> {
    records(&output.stdout)
        .into_iter()
        .filter(|row| row["type"] == "replay-selection")
        .map(|row| {
            (
                row["selection"]["manifest_sha256"].as_str().unwrap().into(),
                row["selection"].clone(),
            )
        })
        .collect()
}

fn legacy_name(project: &Project) -> String {
    // This fixture's absolute path contains only the legacy slash separator
    // outside the allowed label alphabet. Production naming remains shared.
    let component = project
        .0
        .join("fixture-agent")
        .to_str()
        .unwrap()
        .replace('/', "-");
    format!("benchmark-{component}-bug-fix-fixture")
}

fn registered_project(concurrency: usize) -> Project {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.config(concurrency);
    let original_agent = project.0.join("fixture-agent");
    let executable = project.0.join("tools/pi/bin/pi");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    let script = fs::read_to_string(&original_agent).unwrap().replace(
        "root = pathlib.Path(__file__).parent",
        &format!(
            "root = pathlib.Path({})",
            serde_json::to_string(project.0.to_str().unwrap()).unwrap()
        ),
    ).replace("if (root/'reverse-arrivals').exists()", "if batch == 1 and (root/'hold-first-batch').exists():\n    hold_deadline = time.monotonic()+45\n    while not (root/'release-first-batch').exists():\n        assert time.monotonic()<hold_deadline, 'fixture owner did not release first batch'\n        time.sleep(.005)\nif (root/'reverse-arrivals').exists()");
    let script = script.replace("if (root/'reverse-arrivals').exists()", "if (root/f'hold-batch-{batch}').exists():\n    hold_deadline = time.monotonic()+45\n    while not (root/f'release-batch-{batch}').exists():\n        assert time.monotonic()<hold_deadline, 'fixture owner did not release batch'\n        time.sleep(.005)\nif (root/'reverse-arrivals').exists()");
    fixture_writer::write_file(&project.0, &executable, script).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let config_path = project.0.join("benchmark.toml");
    let configuration = fs::read_to_string(&config_path)
        .unwrap()
        .replace(
            &format!("agent='{}'", original_agent.display()),
            "agent='pi'",
        )
        .replace("provider='fixture'", "provider='openrouter'");
    fs::write(config_path, configuration).unwrap();
    fs::write(project.0.join("sweep.toml"), "configs=['benchmark.toml']\n").unwrap();
    project
}

#[test]
fn continuation_metadata_recovery_cli_rejects_execution_options_without_provider_setup() {
    let project = registered_project(1);
    for arguments in [
        vec!["--recover-continuation"],
        vec!["--resume", "--recover-continuation", "--record"],
        vec!["--resume", "--recover-continuation", "--replay"],
        vec!["--resume", "--recover-continuation", "--repeat", "2"],
        vec!["--resume", "--recover-continuation", "--jobs", "2"],
    ] {
        let rejected = sweep(&project, None, &arguments);
        assert!(!rejected.status.success(), "{arguments:?}");
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("--recover-continuation"));
        assert!(!project.0.join("sweep-results").exists());
    }
    let missing = sweep(&project, None, &["--resume", "--recover-continuation"]);
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("no retained sweep to recover"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    assert!(!project.0.join("attempts.jsonl").exists());
}

#[test]
fn campaign_input_seals_stay_stable_after_record_publication_and_offline_replay() {
    let project = registered_project(2);
    let provider = RecordingProvider::new();
    let recorded = sweep(
        &project,
        Some(&provider),
        &["--record", "--repeat", "2", "--cassette-root", "recordings"],
    );
    assert!(
        recorded.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&recorded.stdout),
        String::from_utf8_lossy(&recorded.stderr)
    );
    let seal_path = project.0.join("sweep-results/.checkpoint/inputs.json");
    let check = |replay: bool| {
        let seal: Value = serde_json::from_slice(&fs::read(&seal_path).unwrap()).unwrap();
        assert_eq!(seal["status"], "stable", "{seal}");
        assert_eq!(seal["initial"], seal["final"]);
        let roots = seal["roots"].as_array().unwrap();
        assert!(roots.iter().any(|root| root["name"] == "campaign-0-plan"));
        assert_eq!(
            roots.iter().any(|root| root["name"]
                .as_str()
                .unwrap()
                .starts_with("campaign-0-replay-")),
            replay
        );
        assert!(
            roots
                .iter()
                .all(|root| !root["name"].as_str().unwrap().starts_with("replay-source-"))
        );
        let recording = project
            .0
            .join("recordings/000-benchmark/cassettes/recording.asb.zip");
        assert!(roots.iter().all(|root| root["path"] != json!(recording)));
        for slot in 1..=4 {
            let receipt: Value = serde_json::from_slice(
                &fs::read(project.0.join(format!(
                    "sweep-results/.checkpoint/campaign-completed/000000/{slot:06}.json"
                )))
                .unwrap(),
            )
            .unwrap();
            assert_eq!(receipt["execution"]["slot"], slot);
            assert_eq!(receipt["configuration"], 0);
            assert_eq!(receipt["input_digest"], seal["initial"]["digest"]);
            assert!(receipt["roots"].as_array().unwrap().iter().all(|root| {
                root["path"] != json!(recording)
                    && !root["path"].as_str().unwrap().ends_with(".jsonl.partial")
            }));
            assert_eq!(
                receipt["roots"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|root| root["name"] == "recording-association"),
                !replay
            );
        }
    };
    check(false);
    let assert_reused = |provider: Option<&RecordingProvider>, mode: &str| {
        let mut paths = vec![
            project.0.join("attempts.jsonl"),
            project.0.join("sweep-results/plan.json"),
            seal_path.clone(),
        ];
        for slot in 1..=4 {
            paths.push(project.0.join(format!(
                "sweep-results/.checkpoint/campaign-completed/000000/{slot:06}.json"
            )));
        }
        let before: Vec<_> = paths.iter().map(|path| fs::read(path).unwrap()).collect();
        let rejected = sweep(
            &project,
            provider,
            &[mode, "--resume", "--cassette-root", "other-recordings"],
        );
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("saved campaign source"));
        for (option, value) in [("--repeat", "3"), ("--seed", "999"), ("--track", "1")] {
            if mode != "--replay" && option != "--repeat" {
                continue;
            }
            let rejected = sweep(&project, provider, &[mode, "--resume", option, value]);
            assert!(!rejected.status.success());
            assert!(String::from_utf8_lossy(&rejected.stderr).contains("saved campaign selection"));
        }
        for arguments in [
            vec![mode, "--resume"],
            vec![
                mode,
                "--resume",
                "--repeat",
                "2",
                "--cassette-root",
                "recordings",
            ],
        ] {
            let output = sweep(&project, provider, &arguments);
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let rows = records(&output.stdout);
            assert_eq!(rows.len(), 4);
            assert!(rows.iter().all(|row| row["reused"] == true));
        }
        for (path, bytes) in paths.iter().zip(before) {
            assert!(
                fs::read(path).unwrap() == bytes,
                "historical bytes changed: {}",
                path.display()
            );
        }
    };
    assert_reused(Some(&provider), "--record");
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 4);
    drop(provider);
    let replayed = sweep(
        &project,
        None,
        &[
            "--replay",
            "--repeat",
            "2",
            "--cassette-root",
            "recordings",
            "--seed",
            "73",
        ],
    );
    assert!(
        replayed.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&replayed.stdout),
        String::from_utf8_lossy(&replayed.stderr)
    );
    check(true);
    let public_source = project
        .0
        .join("recordings/000-benchmark/cassettes/recording.asb.zip");
    let hidden_source = public_source.with_extension("held-aside");
    fs::rename(&public_source, &hidden_source).unwrap();
    assert_reused(None, "--replay");
    fs::rename(hidden_source, public_source).unwrap();
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        8
    );
    let result_root = project.0.join("sweep-results");
    let index: Value =
        serde_json::from_slice(&fs::read(result_root.join("plan.json")).unwrap()).unwrap();
    let campaign = result_root.join(index["entries"][0]["campaign"]["root"].as_str().unwrap());
    let reference_path = campaign.join("batch-2/outcomes/2.json");
    let reference: Value = serde_json::from_slice(&fs::read(&reference_path).unwrap()).unwrap();
    let outcome = campaign.join(reference["path"].as_str().unwrap());
    let directory = outcome.parent().unwrap();
    let attempts = fs::read(project.0.join("attempts.jsonl")).unwrap();
    let report_path = project.0.join("results/sweep-comparison/comparison.json");
    let report = fs::read(&report_path).unwrap();
    for case in ["duplicate", "orphan", "temporary"] {
        let extra = directory.join(if case == "temporary" {
            ".unfinished.tmp"
        } else {
            "duplicate.json"
        });
        let held = campaign.join("held-outcome-reference");
        match case {
            "duplicate" => {
                fs::copy(&outcome, &extra).unwrap();
            }
            "orphan" => fs::rename(&reference_path, &held).unwrap(),
            "temporary" => fs::write(&extra, b"unfinished").unwrap(),
            _ => unreachable!(),
        }
        let rejected = sweep(&project, None, &["--replay", "--resume"]);
        assert!(!rejected.status.success(), "{case}");
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("explicit recovery"),
            "{case}: {}",
            String::from_utf8_lossy(&rejected.stderr)
        );
        assert!(
            rejected.stdout.is_empty(),
            "no earlier result presentation before global admission: {case}"
        );
        assert!(
            fs::read(project.0.join("attempts.jsonl")).unwrap() == attempts,
            "{case}"
        );
        assert!(fs::read(&report_path).unwrap() == report, "{case}");
        if case == "orphan" {
            fs::rename(&held, &reference_path).unwrap();
        } else {
            fs::remove_file(&extra).unwrap();
        }
    }
    assert_reused(None, "--replay");
    fs::write(
        project.0.join("grader-error-first"),
        "one unknown grade, no retry",
    )
    .unwrap();
    let partial = sweep(
        &project,
        None,
        &[
            "--replay",
            "--repeat",
            "2",
            "--cassette-root",
            "recordings",
            "--seed",
            "73",
        ],
    );
    assert!(!partial.status.success());
    let sealed = project
        .0
        .join("sweep-results/.checkpoint/campaign-completed/000000");
    assert_eq!(
        fs::read_dir(sealed).unwrap().count(),
        3,
        "{}\n{}",
        String::from_utf8_lossy(&partial.stdout),
        String::from_utf8_lossy(&partial.stderr)
    );
    assert!(
        project
            .0
            .join("sweep-results/.checkpoint/campaign-unsettled/000000/000001.json")
            .is_file()
    );
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        12
    );
    let rejected = sweep(&project, None, &["--replay", "--resume"]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("saved-slot validation"));
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        12,
        "unknown grades are recovery-required, never implicit retries"
    );
}

#[test]
fn interrupted_modern_sweep_keeps_genuine_first_batch_receipts_and_untouched_next_batch() {
    check_interrupted_continuation("legacy", false);
}

#[test]
fn interrupted_modern_sweep_resumes_saved_zip_choices_with_measurements() {
    check_interrupted_continuation("zip", true);
}

#[test]
fn interrupted_modern_sweep_discloses_pending_incomplete_selection_before_execution() {
    check_interrupted_continuation("incomplete", false);
}

#[test]
fn interrupted_modern_sweep_continues_live_batches_without_retrying_old_clients() {
    check_interrupted_continuation("live", true);
}

#[test]
fn interrupted_recording_sweep_imports_old_tracks_and_only_runs_pending_clients() {
    check_interrupted_continuation("record", true);
}

#[test]
fn interrupted_recording_sweep_recovers_publication_without_clients() {
    check_interrupted_continuation("record-ledger-fault", false);
}

#[test]
fn successive_recording_continuations_preserve_all_generations_and_publish_complete_union() {
    check_interrupted_generations("record", true, true);
}

#[test]
fn successive_seeded_replay_continuations_preserve_every_choice_and_measurement() {
    check_interrupted_generations("zip", true, true);
}

fn check_interrupted_continuation(source_kind: &str, measured: bool) {
    check_interrupted_generations(source_kind, measured, false);
}

fn check_interrupted_generations(source_kind: &str, measured: bool, successive: bool) {
    use std::process::Stdio;
    let project = registered_project(4);
    let recording = source_kind.starts_with("record");
    let ledger_fault = source_kind == "record-ledger-fault";
    let total = if successive { 12 } else { 8 };
    if successive {
        fs::write(
            project.0.join("hold-batch-2"),
            "preinstalled bounded batch barrier",
        )
        .unwrap();
    }
    if ledger_fault {
        fs::write(
            project.0.join("block-union-ledger"),
            "fixture-only publication fault",
        )
        .unwrap();
    }
    fs::write(
        project.0.join("hold-first-batch"),
        "bounded fixture barrier",
    )
    .unwrap();
    if measured {
        let path = project.0.join("benchmark.toml");
        let configuration = fs::read_to_string(&path).unwrap();
        fs::write(path, format!("{configuration}\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=[]\n")).unwrap();
    }
    let provider = (recording || source_kind == "live").then(RecordingProvider::new);
    fs::write(
        project.0.join("false-grade-first"),
        "keep a genuine false grade",
    )
    .unwrap();
    let input = project.0.join("replay-input/000-benchmark/cassettes");
    fs::create_dir_all(&input).unwrap();
    let source_path = if matches!(source_kind, "zip" | "incomplete") {
        let source = registered_project(2);
        let provider = RecordingProvider::new();
        let recorded = record(&source, &provider, Some("2"));
        assert!(
            recorded.status.success(),
            "{}",
            String::from_utf8_lossy(&recorded.stderr)
        );
        if source_kind == "incomplete" {
            rewrite_bundle(&source, |manifest, _| {
                manifest.tracks[0].complete = false;
            });
        }
        let destination = input.join("recording.asb.zip");
        fs::copy(source.0.join("new-recording.jsonl"), &destination).unwrap();
        Some(destination)
    } else if source_kind == "legacy" {
        let destination = input.join("benchmark-pi-bug-fix-fixture.jsonl");
        fs::copy(project.0.join("input.jsonl"), &destination).unwrap();
        Some(destination)
    } else {
        None
    };
    let mut command = Command::new(env!("CARGO_BIN_EXE_asb"));
    command.current_dir(&project.0).isolated_env();
    retain_host_rust_toolchain(&mut command);
    command
        .env("HOME", &project.0)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args([
            "sweep",
            "sweep.toml",
            "--repeat",
            if successive { "3" } else { "2" },
            "--jobs",
            "1",
            "--json",
        ]);
    if let Some(provider) = &provider {
        command
            .env("ASB_PROVIDER_BASE_URL", &provider.endpoint)
            .env("OPENROUTER_API_KEY", "fixture-only-key")
            .env("OPENROUTER_BASE_URL", format!("{}/v1", provider.endpoint))
            .env(
                "OPENROUTER_MODELS_URL",
                format!("{}/models", provider.endpoint),
            );
        if recording {
            command.arg("--record");
        }
    } else {
        command.args(["--replay", "--cassette-root", "replay-input"]);
        if matches!(source_kind, "zip" | "incomplete") {
            command.args(["--seed", "73"]);
        }
        if source_kind == "incomplete" {
            command.args(["--track", "1"]);
        }
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut source = Prelude::new(&mut child);
    let event = source.event(&mut child);
    assert_eq!(event["type"], "execution-plan", "{event}");
    // Wait until actual clients have started, not merely the execution-plan
    // prelude (ZIP selection or measured preflight may still emit output).
    let deadline = source.deadline;
    while !(1..=4).all(|replica| project.0.join(format!("barrier-1-{replica}")).exists()) {
        if child.try_wait().unwrap().is_some() || std::time::Instant::now() >= deadline {
            let _ = child.kill();
            drop(source);
            let output = child.wait_with_output().unwrap();
            panic!(
                "clients never reached first batch: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    // The shared four-way batch still settles after presentation fails. This
    // is an actual supported broken-pipe route, not erased start observations.
    drop(source);
    fs::write(
        project.0.join("release-first-batch"),
        "continue owned clients",
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let plan = PathBuf::from(event["path"].as_str().unwrap());
    let root = plan.parent().unwrap();
    let saved: Value = serde_json::from_slice(&fs::read(&plan).unwrap()).unwrap();
    assert!(!root.join("batch-2").exists());
    let attempts = records(&fs::read(project.0.join("attempts.jsonl")).unwrap());
    assert_eq!(attempts.len(), 4);
    assert!(attempts.iter().all(|attempt| attempt["batch"] == 1));
    let checkpoint = project.0.join("sweep-results/.checkpoint");
    let seal: Value =
        serde_json::from_slice(&fs::read(checkpoint.join("inputs.json")).unwrap()).unwrap();
    assert_eq!(seal["status"], "stable", "{seal}");
    assert_eq!(seal["initial"], seal["final"]);
    assert!(checkpoint.join("campaign-admission.json").is_file());
    assert!(
        checkpoint
            .join("campaign-reservations/000000.json")
            .is_file()
    );
    let mut false_grades = 0;
    let mut frozen = Vec::new();
    if recording {
        let path = PathBuf::from(saved["recording"].as_str().unwrap());
        frozen.push((path.clone(), fs::read(path).unwrap()));
    }
    for id in 1..=4 {
        let receipt = checkpoint.join(format!("campaign-completed/000000/{id:06}.json"));
        assert!(
            receipt.is_file(),
            "missing genuine receipt {id}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reference: Value = serde_json::from_slice(
            &fs::read(root.join(format!("batch-1/outcomes/{id}.json"))).unwrap(),
        )
        .unwrap();
        let outcome: Value = serde_json::from_slice(
            &fs::read(root.join(reference["path"].as_str().unwrap())).unwrap(),
        )
        .unwrap();
        assert_eq!(outcome["summary"]["execution"]["slot"], id);
        assert_eq!(outcome["workload_cleanup_confirmed"], true);
        assert_eq!(outcome["proxy_cleanup_confirmed"], true);
        if outcome["summary"]["workload_success"] == false {
            false_grades += 1;
        }
        if recording {
            let destination = PathBuf::from(
                outcome["recording_track"]["track"]["destination"]
                    .as_str()
                    .unwrap(),
            );
            let name = destination.file_name().unwrap().to_str().unwrap();
            let association = destination
                .parent()
                .unwrap()
                .join(format!(".{name}.partial/association-{id}.json"));
            frozen.push((association.clone(), fs::read(association).unwrap()));
        }
        frozen.push((receipt.clone(), fs::read(receipt).unwrap()));
    }
    assert_eq!(false_grades, 1);
    for id in 5..=8 {
        assert!(
            !checkpoint
                .join(format!("campaign-completed/000000/{id:06}.json"))
                .exists()
        );
    }
    // Freeze actual original evidence, not merely receipt names. Continue from
    // the private inputs after removing this fixture's public replay source.
    for path in [
        project.0.join("sweep-results/plan.json"),
        checkpoint.join("inputs.json"),
        root.join("plan.json"),
    ] {
        frozen.push((path.clone(), fs::read(path).unwrap()));
    }
    for id in 1..=4 {
        for name in ["starts", "outcomes"] {
            let path = root.join(format!("batch-1/{name}/{id}.json"));
            frozen.push((path.clone(), fs::read(path).unwrap()));
        }
        let path = root.join(format!("batch-1/outcomes/{id}.json"));
        let reference: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let path = root.join(reference["path"].as_str().unwrap());
        frozen.push((path.clone(), fs::read(path).unwrap()));
    }
    for slot in saved["slots"].as_array().unwrap().iter().take(4) {
        for path in [
            root.join(slot["log"].as_str().unwrap()),
            root.join(slot["workspace"].as_str().unwrap())
                .join("src/parser.py"),
        ] {
            frozen.push((path.clone(), fs::read(path).unwrap()));
        }
        if measured {
            let path = root
                .join(slot["measurements"].as_str().unwrap())
                .join("run.json");
            frozen.push((path.clone(), fs::read(path).unwrap()));
        }
    }
    // Invalid old evidence must prevent ALL new admission and publication.
    let last = checkpoint.join("campaign-completed/000000/000004.json");
    let original = fs::read(&last).unwrap();
    fs::write(&last, b"{}").unwrap();
    let arguments: &[&str] = if recording {
        &["--record", "--resume"]
    } else if provider.is_some() {
        &["--resume"]
    } else {
        &["--replay", "--resume"]
    };
    let rejected = sweep(&project, provider.as_ref(), arguments);
    assert!(!rejected.status.success());
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        4
    );
    assert!(!checkpoint.join("campaign-continuation").exists());
    fs::write(last, original).unwrap();
    if let Some(path) = source_path {
        fs::remove_file(path).unwrap();
    }
    if successive {
        let mut child = sweep_command(&project, provider.as_ref(), arguments)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut source = Prelude::new(&mut child);
        // Drain the reused first batch before deliberately closing output.
        // Otherwise a full pipe could stop admission before B2 really started.
        let mut reused = 0;
        while reused < 4 {
            let row = source.event(&mut child);
            reused += usize::from(row["reused"] == true);
        }
        let deadline = source.deadline;
        while !(1..=4).all(|replica| project.0.join(format!("barrier-2-{replica}")).exists()) {
            if child.try_wait().unwrap().is_some() || std::time::Instant::now() >= deadline {
                let _ = child.kill();
                drop(source);
                let output = child.wait_with_output().unwrap();
                panic!(
                    "clients never reached second batch: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        drop(source);
        fs::write(
            project.0.join("release-batch-2"),
            "settle owned second batch",
        )
        .unwrap();
        let interrupted = child.wait_with_output().unwrap();
        assert!(!interrupted.status.success());
        assert_eq!(
            records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
            8
        );
        assert!(!root.join("batch-3").exists());
        for id in 5..=8 {
            let slot = &saved["slots"][id - 1];
            let outcome_path = root.join(format!("batch-2/outcomes/{}.json", id - 4));
            let reference: Value =
                serde_json::from_slice(&fs::read(&outcome_path).unwrap()).unwrap();
            for path in [
                checkpoint.join(format!("campaign-completed/000000/{id:06}.json")),
                root.join(format!("batch-2/starts/{}.json", id - 4)),
                outcome_path,
                root.join(reference["path"].as_str().unwrap()),
                root.join(slot["log"].as_str().unwrap()),
                root.join(slot["workspace"].as_str().unwrap())
                    .join("src/parser.py"),
            ] {
                frozen.push((path.clone(), fs::read(path).unwrap()));
            }
            if measured {
                let path = root
                    .join(slot["measurements"].as_str().unwrap())
                    .join("run.json");
                frozen.push((path.clone(), fs::read(path).unwrap()));
            }
        }
        for path in [
            checkpoint.join("campaign-continuation/admitted.json"),
            checkpoint.join("campaign-continuation/000000.json"),
        ] {
            frozen.push((path.clone(), fs::read(path).unwrap()));
        }
        if recording {
            let manifest: Value = serde_json::from_slice(
                &fs::read(checkpoint.join("campaign-continuation/000000.json")).unwrap(),
            )
            .unwrap();
            for path in [
                PathBuf::from(manifest["recording"]["destination"].as_str().unwrap()),
                root.join("recording-current.json"),
            ] {
                frozen.push((path.clone(), fs::read(path).unwrap()));
            }
        }
        // Faults in the established ancestor must stop before a successor stage
        // or paid attempt. Restore only fixture-owned fault injections afterward.
        let mut faults = vec![
            checkpoint.join("campaign-continuation/admitted.json"),
            checkpoint.join("campaign-completed/000000/000004.json"),
        ];
        if recording {
            faults.push(root.join("recording-current.json"));
        }
        let report = project.0.join("results/sweep-comparison/comparison.json");
        let report_before = fs::read(&report).unwrap();
        for path in faults {
            let original = fs::read(&path).unwrap();
            let retained = checkpoint.join("retained-ancestor-fault");
            fs::rename(&path, &retained).unwrap();
            let rejected = sweep(&project, provider.as_ref(), arguments);
            // Restore the same file, including its private publication mode;
            // recreating with write() would change 0600 to the host umask mode
            // and correctly invalidate historical content fingerprints.
            fs::rename(&retained, &path).unwrap();
            assert_eq!(fs::read(&path).unwrap(), original);
            assert!(
                !rejected.status.success(),
                "missing ancestor {}",
                path.display()
            );
            assert_eq!(
                records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
                8
            );
            assert_eq!(fs::read(&report).unwrap(), report_before);
            assert!(
                !checkpoint
                    .join("campaign-continuation/successors/000001")
                    .exists()
            );
        }
        let orphan = checkpoint.join("campaign-continuation/successors/000001");
        fs::create_dir_all(&orphan).unwrap();
        fs::write(
            orphan.join("admitted.json"),
            b"fixture-owned orphan must be retained",
        )
        .unwrap();
        let rejected = sweep(&project, provider.as_ref(), arguments);
        assert!(!rejected.status.success());
        assert_eq!(
            records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
            8
        );
        assert_eq!(
            fs::read(orphan.join("admitted.json")).unwrap(),
            b"fixture-owned orphan must be retained"
        );
        assert_eq!(fs::read(&report).unwrap(), report_before);
        fs::remove_file(orphan.join("admitted.json")).unwrap();
        fs::remove_dir(orphan).unwrap();
    }
    // Provider-only legacy input is intentionally retained but not reusable
    // once its unsealed pending slots require recovery.
    for iteration in 0..if source_kind == "legacy" { 1 } else { 2 } {
        let resumed = sweep(&project, provider.as_ref(), arguments);
        // A historical provider-only JSONL cassette has no sealed local-tool
        // transcript. It may retain its already graded first batch, but it
        // must not admit a fresh batch that could escape the sealed replay
        // contract. Modern recordings continue with their four batch-two
        // attempts despite the genuine false grade.
        let legacy_provider_only = source_kind == "legacy";
        let expected_total = if legacy_provider_only { 4 } else { total };
        assert!(!resumed.status.success());
        let rows = records(&resumed.stdout)
            .into_iter()
            .filter(|row| row.get("task").is_some())
            .collect::<Vec<_>>();
        assert_eq!(
            rows.len(),
            expected_total,
            "iteration {iteration}: {}\n{}",
            String::from_utf8_lossy(&resumed.stdout),
            String::from_utf8_lossy(&resumed.stderr)
        );
        if saved["replay_selection"].is_object() {
            let expected = saved["replay_selection"]["tracks"].as_array().unwrap();
            for row in &rows {
                let slot = row["execution"]["slot"].as_u64().unwrap() as usize;
                assert_eq!(row["execution"]["replay_track"], expected[slot - 1]);
                assert_eq!(row["execution"]["replay_seed"], 73);
                assert_eq!(
                    row["execution"]["replay_algorithm"],
                    saved["replay_selection"]["algorithm"]
                );
            }
        }
        if source_kind == "incomplete" && iteration == 0 {
            let events = records(&resumed.stdout);
            let warning = events
                .iter()
                .position(|row| row["code"] == "incomplete-track")
                .expect("disclose incomplete pending track");
            let execution = events
                .iter()
                .position(|row| row["task"].is_object() && row["reused"] != true)
                .unwrap();
            assert!(warning < execution);
        }
        assert_eq!(
            rows.iter()
                .filter(|row| row["workload_success"] == false)
                .count(),
            1
        );
        assert_eq!(
            rows.iter().filter(|row| row["reused"] == true).count(),
            if legacy_provider_only || iteration > 0 {
                expected_total
            } else {
                expected_total - 4
            }
        );
        let attempts = records(&fs::read(project.0.join("attempts.jsonl")).unwrap());
        assert_eq!(attempts.len(), expected_total);
        assert_eq!(
            attempts
                .iter()
                .filter(|attempt| attempt["batch"] == 2)
                .count(),
            if legacy_provider_only { 0 } else { 4 }
        );
        if matches!(source_kind, "zip" | "incomplete") {
            for attempt in &attempts {
                let index = ((attempt["batch"].as_u64().unwrap() - 1) * 4
                    + attempt["replica"].as_u64().unwrap()
                    - 1) as usize;
                let input_index = saved["slots"][index]["replay_input"].as_u64().unwrap() as usize;
                let path = root.join(
                    saved["replay_inputs"][input_index]["private"]
                        .as_str()
                        .unwrap(),
                );
                let tape = records(&fs::read(path).unwrap());
                let response: Value =
                    serde_json::from_str(tape[0]["response"]["body"].as_str().unwrap()).unwrap();
                assert_eq!(attempt["response_tag"], response["recorded_track"]);
            }
        }
        let report: Value = serde_json::from_slice(
            &fs::read(project.0.join("results/sweep-comparison/comparison.json")).unwrap(),
        )
        .unwrap();
        let reported = report["benchmarks"].as_array().unwrap();
        // The report retains the complete admitted campaign, including its
        // untouched pending slots, even when a provider-only legacy cassette
        // is not eligible to admit those slots for sealed replay.
        assert_eq!(reported.len(), total);
        let mut ids = std::collections::BTreeSet::new();
        for row in reported {
            if legacy_provider_only && row["execution"].is_null() {
                assert_eq!(row["id"]["batch"], 2);
                assert_eq!(row["result_source"], "missing");
                continue;
            }
            let slot = row["execution"]["slot"].as_u64().unwrap_or_else(|| {
                panic!(
                    "missing report execution: {row}\nresume stderr: {}",
                    String::from_utf8_lossy(&resumed.stderr)
                )
            });
            assert!(ids.insert(slot));
            assert_eq!(row["execution_cohort"]["selected_replicas"], 4);
            assert_eq!(row["execution_cohort"]["dispatch"]["configured"], 4);
            assert_eq!(row["execution_cohort"]["dispatch"]["admitted"], 4);
        }
        if measured {
            for slot in saved["slots"].as_array().unwrap().iter().skip(4) {
                let path = root
                    .join(slot["measurements"].as_str().unwrap())
                    .join("run.json");
                let observation: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                assert_eq!(observation["execution"]["slot"], slot["id"]);
                assert_eq!(observation["execution_cohort"]["dispatch"]["admitted"], 4);
                if iteration == 0 {
                    frozen.push((path.clone(), fs::read(path).unwrap()));
                }
            }
        }
        for (path, bytes) in &frozen {
            assert_eq!(
                &fs::read(path).unwrap(),
                bytes,
                "changed {}",
                path.display()
            );
        }
        if !legacy_provider_only {
            for id in 5..=total {
                let receipt = checkpoint.join(format!("campaign-completed/000000/{id:06}.json"));
                assert!(
                    receipt.is_file(),
                    "missing new receipt {id}: {}",
                    String::from_utf8_lossy(&resumed.stderr)
                );
                if iteration == 0 {
                    frozen.push((receipt.clone(), fs::read(receipt).unwrap()));
                }
            }
        }
        if let Some(provider) = &provider {
            assert_eq!(
                provider.calls.load(std::sync::atomic::Ordering::SeqCst),
                total
            );
        }
        if recording {
            let descriptor: Value = serde_json::from_slice(
                &fs::read(checkpoint.join(if successive {
                    "campaign-continuation/successors/000001/000000.json"
                } else {
                    "campaign-continuation/000000.json"
                }))
                .unwrap(),
            )
            .unwrap();
            let path = PathBuf::from(descriptor["recording"]["destination"].as_str().unwrap());
            let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
            let union: Value =
                serde_json::from_reader(archive.by_name("manifest.json").unwrap()).unwrap();
            let tracks = union["tracks"].as_array().unwrap();
            assert_eq!(tracks.len(), total);
            assert_eq!(
                union["provenance"]["imports"].as_array().unwrap().len(),
                total - 4
            );
            assert_ne!(union["recording_id"], saved["campaign"]);
            let original_path = saved["recording"].as_str().unwrap();
            let mut original =
                zip::ZipArchive::new(std::fs::File::open(original_path).unwrap()).unwrap();
            let old: Value =
                serde_json::from_reader(original.by_name("manifest.json").unwrap()).unwrap();
            assert_eq!(&tracks[..4], old["tracks"].as_array().unwrap().as_slice());
            if successive {
                let ancestor: Value = serde_json::from_slice(
                    &fs::read(checkpoint.join("campaign-continuation/000000.json")).unwrap(),
                )
                .unwrap();
                let mut prior = zip::ZipArchive::new(
                    fs::File::open(ancestor["recording"]["destination"].as_str().unwrap()).unwrap(),
                )
                .unwrap();
                let prior_manifest: Value =
                    serde_json::from_reader(prior.by_name("manifest.json").unwrap()).unwrap();
                assert_eq!(
                    &tracks[..8],
                    prior_manifest["tracks"].as_array().unwrap().as_slice()
                );
                for id in 1..=8 {
                    let mut actual = Vec::new();
                    let mut expected = Vec::new();
                    std::io::Read::read_to_end(
                        &mut archive.by_name(&format!("tracks/{id}.jsonl")).unwrap(),
                        &mut actual,
                    )
                    .unwrap();
                    std::io::Read::read_to_end(
                        &mut prior.by_name(&format!("tracks/{id}.jsonl")).unwrap(),
                        &mut expected,
                    )
                    .unwrap();
                    assert_eq!(actual, expected, "imported ancestor slot {id}");
                }
                let admission: Value = serde_json::from_slice(
                    &fs::read(
                        checkpoint.join("campaign-continuation/successors/000001/admitted.json"),
                    )
                    .unwrap(),
                )
                .unwrap();
                let path = root
                    .join(".asb-generations")
                    .join(admission["generation"].as_str().unwrap())
                    .join("recording-current.json");
                assert!(path.is_file());
                if iteration == 0 {
                    frozen.push((path.clone(), fs::read(path).unwrap()));
                }
            }
            for id in 1..=total {
                assert_eq!(tracks[id - 1]["id"], id);
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(
                    &mut archive.by_name(&format!("tracks/{id}.jsonl")).unwrap(),
                    &mut bytes,
                )
                .unwrap();
                assert!(!bytes.is_empty());
                if id <= 4 {
                    let mut old_bytes = Vec::new();
                    std::io::Read::read_to_end(
                        &mut original.by_name(&format!("tracks/{id}.jsonl")).unwrap(),
                        &mut old_bytes,
                    )
                    .unwrap();
                    assert_eq!(bytes, old_bytes);
                }
            }
            if ledger_fault && iteration == 0 {
                let blocker = root.join("recording-current.json");
                assert!(blocker.is_dir());
                fs::rename(&blocker, root.join("retained-ledger-blocker")).unwrap();
                let index_before = fs::read(project.0.join("sweep-results/plan.json")).unwrap();
                let report_path = project.0.join("results/sweep-comparison/comparison.json");
                let report_before = fs::read(&report_path).unwrap();
                let mut history_before: Vec<_> = fs::read_dir(project.0.join("sweep-history"))
                    .map(|entries| entries.map(|entry| entry.unwrap().file_name()).collect())
                    .unwrap_or_default();
                history_before.sort();
                let refused = sweep(&project, None, &["--replay", "--repeat", "1"]);
                assert!(!refused.status.success());
                assert!(
                    String::from_utf8_lossy(&refused.stderr).contains("recover-recording"),
                    "{}",
                    String::from_utf8_lossy(&refused.stderr)
                );
                assert_eq!(
                    records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
                    8
                );
                assert_eq!(
                    fs::read(project.0.join("sweep-results/plan.json")).unwrap(),
                    index_before
                );
                assert_eq!(fs::read(&report_path).unwrap(), report_before);
                let mut history_after: Vec<_> = fs::read_dir(project.0.join("sweep-history"))
                    .map(|entries| entries.map(|entry| entry.unwrap().file_name()).collect())
                    .unwrap_or_default();
                history_after.sort();
                assert_eq!(history_after, history_before);
                let archive_before =
                    fs::read(descriptor["recording"]["destination"].as_str().unwrap()).unwrap();
                for _ in 0..2 {
                    let recovered = sweep(
                        &project,
                        None,
                        &["--record", "--resume", "--recover-recording"],
                    );
                    assert!(
                        recovered.status.success(),
                        "{}\n{}",
                        String::from_utf8_lossy(&recovered.stdout),
                        String::from_utf8_lossy(&recovered.stderr)
                    );
                    let events = records(&recovered.stdout);
                    assert_eq!(events.len(), 1);
                    assert_eq!(events[0]["agent_executions"], 0);
                    assert_eq!(events[0]["evaluations"], 0);
                    assert_eq!(
                        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
                        8
                    );
                    assert_eq!(
                        fs::read(descriptor["recording"]["destination"].as_str().unwrap()).unwrap(),
                        archive_before
                    );
                }
            }
            assert!(root.join("recording-current.json").is_file());
            if measured && iteration == 0 {
                check_union_measurement_only_owner(&project, root, &saved);
            }
        }
    }
    if recording {
        let compared = compare_campaign(&project, &root.join("plan.json"));
        assert!(
            compared.status.success(),
            "{}",
            String::from_utf8_lossy(&compared.stderr)
        );
        drop(provider);
        let descriptor: Value = serde_json::from_slice(
            &fs::read(checkpoint.join(if successive {
                "campaign-continuation/successors/000001/000000.json"
            } else {
                "campaign-continuation/000000.json"
            }))
            .unwrap(),
        )
        .unwrap();
        let union = descriptor["recording"]["destination"].as_str().unwrap();
        let original = PathBuf::from(saved["recording"].as_str().unwrap());
        let hidden = original.with_extension("temporarily-unavailable");
        fs::rename(&original, &hidden).unwrap();
        // Replay both an imported and a newly captured track with no provider
        // listener, no credentials and no ancestor ZIP at its original path.
        for track in if successive {
            vec!["1", "5", "9"]
        } else {
            vec!["1", "5"]
        } {
            let replayed = replay_from(&project, Some(union), &["--track", track, "--repeat", "1"]);
            let rows = records(&replayed.stdout)
                .into_iter()
                .filter(|row| row["task"].is_object())
                .collect::<Vec<_>>();
            assert_eq!(
                rows.len(),
                4,
                "{}\n{}",
                String::from_utf8_lossy(&replayed.stdout),
                String::from_utf8_lossy(&replayed.stderr)
            );
            assert!(
                rows.iter()
                    .all(|row| row["execution"]["replay_track"] == track.parse::<u64>().unwrap())
            );
            assert!(rows.iter().all(|row| row["process_success"] == true));
        }
        fs::rename(hidden, original).unwrap();
        fs::remove_file(project.0.join("block-union-ledger")).ok();
        let before = records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len();
        let replayed = sweep(
            &project,
            None,
            &[
                "--replay",
                "--track",
                if successive { "9" } else { "5" },
                "--repeat",
                "1",
            ],
        );
        assert_eq!(
            records(&replayed.stdout)
                .into_iter()
                .filter(|row| row["task"].is_object())
                .count(),
            4,
            "{}\n{}",
            String::from_utf8_lossy(&replayed.stdout),
            String::from_utf8_lossy(&replayed.stderr)
        );
        assert_eq!(
            records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
            before + 4
        );
        let current: Value =
            serde_json::from_slice(&fs::read(project.0.join("sweep-results/plan.json")).unwrap())
                .unwrap();
        let historical = project
            .0
            .join("sweep-history")
            .join(current["run_id"].as_str().unwrap())
            .join("sweep-results")
            .join(root.strip_prefix(project.0.join("sweep-results")).unwrap())
            .join("plan.json");
        let compared = compare_campaign(&project, &historical);
        assert!(
            compared.status.success(),
            "{}\n{}",
            historical.display(),
            String::from_utf8_lossy(&compared.stderr)
        );
        for (path, bytes) in frozen
            .iter()
            .filter(|(path, _)| !path.starts_with(project.0.join("sweep-results")))
        {
            assert_eq!(&fs::read(path).unwrap(), bytes);
        }
    }
}

fn check_union_measurement_only_owner(project: &Project, root: &std::path::Path, saved: &Value) {
    let paths = [
        root.join("batch-2/outcomes/1.json"),
        root.join(saved["slots"][4]["log"].as_str().unwrap()),
    ];
    let hidden = paths
        .iter()
        .map(|path| path.with_extension("union-test-hidden"))
        .collect::<Vec<_>>();
    for (path, hidden) in paths.iter().zip(&hidden) {
        fs::rename(path, hidden).unwrap();
    }
    let compare = || {
        Command::new(env!("CARGO_BIN_EXE_asb"))
            .current_dir(&project.0)
            .isolated_env()
            .env("HOME", &project.0)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .args(["compare", "sweep.toml"])
            .output()
            .unwrap()
    };
    let valid = compare();
    assert!(
        valid.status.success(),
        "{}",
        String::from_utf8_lossy(&valid.stderr)
    );
    let report_path = project.0.join("results/sweep-comparison/comparison.json");
    let before = fs::read(&report_path).unwrap();
    // The measurement and its immutable association stay exact. Change only
    // the actual start's generation: a measurement-only bypass used to avoid
    // consulting that owning context altogether.
    let path = root.join("batch-2/starts/1.json");
    let start = fs::read(&path).unwrap();
    let mut changed: Value = serde_json::from_slice(&start).unwrap();
    changed["generation"]["manifest"]["sha256"] = "0".repeat(64).into();
    fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
    let rejected = compare();
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("generation"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert_eq!(fs::read(&report_path).unwrap(), before);
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        saved["slots"].as_array().unwrap().len()
    );
    fs::write(path, start).unwrap();
    for (path, hidden) in paths.iter().zip(hidden) {
        fs::rename(hidden, path).unwrap();
    }
}

#[test]
fn repeated_sweep_records_one_bundle_per_configuration_and_replays_identically_across_jobs() {
    let project = Project::new();
    project.config(2);
    let config = fs::read_to_string(project.0.join("benchmark.toml"))
        .unwrap()
        .replace("provider='fixture'", "provider='openrouter'");
    for name in ["a.toml", "b.toml"] {
        fs::write(project.0.join(name), &config).unwrap();
    }
    fs::write(
        project.0.join("sweep.toml"),
        "configs=['a.toml','b.toml']\n",
    )
    .unwrap();
    let provider = RecordingProvider::new();
    let recorded = sweep(
        &project,
        Some(&provider),
        &[
            "--record",
            "--repeat",
            "2",
            "--cassette-root",
            "recordings",
            "--jobs",
            "2",
            "--max-active-attempts",
            "4",
        ],
    );
    assert!(
        recorded.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&recorded.stdout),
        String::from_utf8_lossy(&recorded.stderr)
    );
    let index_path = project.0.join("sweep-results/plan.json");
    let index: Value = serde_json::from_slice(&fs::read(&index_path).unwrap()).unwrap();
    assert_eq!(index["schema_version"], 2);
    assert_eq!(index["execution"]["repeat"], 2);
    assert_eq!(index["execution"]["recording_reservation"]["references"], 8);
    let mut reserved = asb_cli::cassette::RecordingReservation::default();
    for (number, name) in ["a", "b"].into_iter().enumerate() {
        assert!(
            index["entries"][number]["campaign"]["root"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{number:03}-{name}/campaigns/"))
        );
        let path = project.0.join(format!(
            "recordings/{number:03}-{name}/cassettes/recording.asb.zip"
        ));
        let archive =
            asb_cli::cassette::Archive::open(&path, asb_cli::cassette::Limits::default()).unwrap();
        assert_eq!(archive.manifest().tracks.len(), 4);
        let campaign = project.0.join("sweep-results").join(
            index["entries"][number]["campaign"]["root"]
                .as_str()
                .unwrap(),
        );
        let retained: Value =
            serde_json::from_slice(&fs::read(campaign.join("plan.json")).unwrap()).unwrap();
        assert_eq!(retained["recording_selection"]["task_id"], "bug-fix");
        let capacity: asb_cli::cassette::RecordingReservation =
            serde_json::from_value(retained["recording_reservation"].clone()).unwrap();
        assert_eq!(capacity.references, 4);
        reserved = reserved.checked_add(&capacity).unwrap();
    }
    assert!(
        project
            .0
            .join("sweep-results/campaigns-published.json")
            .is_file()
    );
    let report_path = project.0.join("results/sweep-comparison/comparison.json");
    let report: Value = serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(report["benchmarks"].as_array().unwrap().len(), 8);
    assert_eq!(report["selection_plan"]["execution_mode"], "record");
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 8);
    assert_eq!(
        serde_json::to_value(&reserved).unwrap(),
        index["execution"]["recording_reservation"]
    );
    let report_bytes = fs::read(&report_path).unwrap();
    let index_bytes = fs::read(&index_path).unwrap();
    for field in [
        "references",
        "reference_bytes",
        "journal_bytes",
        "manifest_bytes",
        "publication_bytes",
        "directory_bytes",
        "schema_version",
    ] {
        let mut changed = index.clone();
        let value = changed["execution"]["recording_reservation"][field]
            .as_u64()
            .unwrap();
        changed["execution"]["recording_reservation"][field] = json!(value + 1);
        fs::write(&index_path, serde_json::to_vec(&changed).unwrap()).unwrap();
        let refused = Command::new(env!("CARGO_BIN_EXE_asb"))
            .current_dir(&project.0)
            .isolated_env()
            .args(["compare", "sweep.toml"])
            .output()
            .unwrap();
        assert!(!refused.status.success(), "accepted changed {field}");
        assert_eq!(fs::read(&report_path).unwrap(), report_bytes);
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 8);
    }
    fs::write(&index_path, index_bytes).unwrap();
    let mut previous = None;
    for jobs in ["1", "2"] {
        let replayed = sweep(
            &project,
            None,
            &[
                "--replay",
                "--repeat",
                "3",
                "--seed",
                "73",
                "--cassette-root",
                "recordings",
                "--jobs",
                jobs,
                "--max-active-attempts",
                "4",
            ],
        );
        assert!(
            replayed.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&replayed.stdout),
            String::from_utf8_lossy(&replayed.stderr)
        );
        let choices = selections(&replayed);
        assert_eq!(choices.len(), 2);
        for selection in choices.values() {
            assert_eq!(selection["seed"], 73);
            assert_eq!(selection["tracks"].as_array().unwrap().len(), 6);
        }
        if let Some(previous) = &previous {
            assert_eq!(previous, &choices);
        }
        previous = Some(choices);
        let report: Value = serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
        let rows = report["benchmarks"].as_array().unwrap();
        assert_eq!(rows.len(), 12);
        let mut identities = std::collections::BTreeSet::new();
        for row in rows {
            assert!(identities.insert(row["id"].to_string()));
            assert_eq!(row["execution"]["replay_seed"], 73);
            assert_eq!(row["execution_cohort"]["selected_replicas"], 2);
        }
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 8);
    }
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        32
    );
    let old_index = fs::read(&index_path).unwrap();
    let old_report = fs::read(&report_path).unwrap();
    fs::remove_file(
        project
            .0
            .join("recordings/001-b/cassettes/recording.asb.zip"),
    )
    .unwrap();
    let refused = sweep(
        &project,
        None,
        &["--replay", "--repeat", "2", "--cassette-root", "recordings"],
    );
    assert!(!refused.status.success());
    assert_eq!(fs::read(index_path).unwrap(), old_index);
    assert_eq!(fs::read(report_path).unwrap(), old_report);
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        32
    );
}

#[test]
fn recording_sweep_global_metadata_preflight_preserves_old_results_without_clients() {
    let project = Project::new();
    project.config(1);
    let config = fs::read_to_string(project.0.join("benchmark.toml"))
        .unwrap()
        .replace("provider='fixture'", "provider='openrouter'")
        .replace("model='fixture'", &format!("model='{}'", "m".repeat(4096)));
    let mut names = Vec::new();
    for index in 0..60 {
        let name = format!("config-{index:02}.toml");
        fs::write(project.0.join(&name), &config).unwrap();
        names.push(name);
    }
    fs::write(
        project.0.join("sweep.toml"),
        format!("configs={}\n", serde_json::to_string(&names).unwrap()),
    )
    .unwrap();
    let index = project.0.join("sweep-results/plan.json");
    let report = project.0.join("results/sweep-comparison/comparison.json");
    fs::create_dir_all(index.parent().unwrap()).unwrap();
    fs::create_dir_all(report.parent().unwrap()).unwrap();
    fs::write(&index, b"preserved original index").unwrap();
    fs::write(&report, b"preserved original report").unwrap();
    let provider = RecordingProvider::new();
    let refused = sweep(&project, Some(&provider), &["--record", "--repeat", "1000"]);
    assert!(!refused.status.success());
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("aggregate metadata budget"), "{message}");
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(!project.0.join("attempts.jsonl").exists());
    assert!(!project.0.join("barrier-1-1").exists());
    assert!(!project.0.join("cassettes").exists());
    assert!(!project.0.join("sweep-history").exists());
    assert_eq!(fs::read(index).unwrap(), b"preserved original index");
    assert_eq!(fs::read(report).unwrap(), b"preserved original report");
}

#[test]
fn default_sweep_bundle_survives_history_and_replay_concurrency_changes() {
    let project = Project::new();
    project.config(1);
    let config_path = project.0.join("benchmark.toml");
    let config = fs::read_to_string(&config_path)
        .unwrap()
        .replace("provider='fixture'", "provider='openrouter'");
    let config = format!(
        "{config}\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=[]\n"
    );
    fs::write(&config_path, &config).unwrap();
    fs::write(project.0.join("sweep.toml"), "configs=['benchmark.toml']\n").unwrap();
    let provider = RecordingProvider::new();
    let recorded = sweep(&project, Some(&provider), &["--record"]);
    assert!(
        recorded.status.success(),
        "{}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    let original_plan = campaign_plan(&recorded);
    let original: Value = serde_json::from_slice(&fs::read(&original_plan).unwrap()).unwrap();
    let recording = PathBuf::from(original["recording"].as_str().unwrap());
    assert!(recording.starts_with(project.0.join("cassettes")));
    fs::write(
        &config_path,
        config.replace("concurrency=1", "concurrency=2"),
    )
    .unwrap();
    fs::write(project.0.join("expected-concurrency"), "2").unwrap();
    let replayed = sweep(
        &project,
        None,
        &["--replay", "--repeat", "2", "--seed", "18"],
    );
    assert!(
        replayed.status.success(),
        "{}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    let replay_index: Value =
        serde_json::from_slice(&fs::read(project.0.join("sweep-results/plan.json")).unwrap())
            .unwrap();
    let preserved = project
        .0
        .join("sweep-history")
        .join(replay_index["run_id"].as_str().unwrap())
        .join("sweep-results")
        .join(
            original_plan
                .strip_prefix(project.0.join("sweep-results"))
                .unwrap(),
        );
    let historical = compare_campaign(&project, &preserved);
    assert!(
        historical.status.success(),
        "{}",
        String::from_utf8_lossy(&historical.stderr)
    );
    let historical_report: Value = serde_json::from_slice(
        &fs::read(
            preserved
                .parent()
                .unwrap()
                .join("comparison/comparison.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        historical_report["benchmarks"][0]["result_source"],
        "measurements"
    );
    assert_eq!(
        historical_report["benchmarks"][0]["execution_cohort"]["selected_replicas"],
        1
    );
    let again = sweep(&project, None, &["--replay"]);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert!(recording.is_file());
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        7
    );
    let index = project.0.join("sweep-results/plan.json");
    let before = fs::read(&index).unwrap();
    // This custom-client fixture intentionally has no complete runtime input
    // closure. Fresh grades/reports work, but absent optional seals cannot be
    // promoted into permission to skip or retry its historical executions.
    for arguments in [
        vec!["--import-grades"],
        vec!["--replay", "--resume"],
        vec!["--replay", "--resume", "--repeat", "2"],
        vec!["--replay", "--resume", "--reconcile-completed"],
    ] {
        assert!(!sweep(&project, None, &arguments).status.success());
        assert_eq!(fs::read(&index).unwrap(), before);
        assert_eq!(
            records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
            7
        );
    }
}

#[test]
fn numeric_legacy_sweep_intent_cannot_relabel_frozen_track_choices() {
    let project = Project::new();
    project.config(1);
    fs::write(project.0.join("sweep.toml"), "configs=['benchmark.toml']\n").unwrap();
    let directory = project.0.join("legacy/000-benchmark/cassettes");
    fs::create_dir_all(&directory).unwrap();
    // Dispatch is content-based, including a legacy JSONL with a ZIP suffix.
    fs::copy(
        project.0.join("input.jsonl"),
        directory.join("recording.asb.zip"),
    )
    .unwrap();
    let replayed = sweep(
        &project,
        None,
        &[
            "--replay",
            "--repeat",
            "2",
            "--track",
            "1",
            "--cassette-root",
            "legacy",
        ],
    );
    assert!(
        replayed.status.success(),
        "{}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    let index_path = project.0.join("sweep-results/plan.json");
    let original = fs::read(&index_path).unwrap();
    let report_path = project.0.join("results/sweep-comparison/comparison.json");
    let report = fs::read(&report_path).unwrap();
    for selector in [json!({"id":2}), json!({"min":"tokens"}), Value::Null] {
        let mut index: Value = serde_json::from_slice(&original).unwrap();
        index["execution"]["selector"] = selector;
        fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
        let compared = Command::new(env!("CARGO_BIN_EXE_asb"))
            .current_dir(&project.0)
            .isolated_env()
            .args(["compare", "sweep.toml"])
            .output()
            .unwrap();
        assert!(!compared.status.success());
        assert_eq!(fs::read(&report_path).unwrap(), report);
    }
    fs::write(index_path, original).unwrap();
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        2
    );
}

#[test]
fn invalid_present_sweep_bundle_never_falls_back_to_valid_legacy_input() {
    let project = Project::new();
    project.config(1);
    fs::write(project.0.join("sweep.toml"), "configs=['benchmark.toml']\n").unwrap();
    let directory = project.0.join("legacy/000-benchmark/cassettes");
    fs::create_dir_all(&directory).unwrap();
    let legacy = directory.join(format!("{}.jsonl", legacy_name(&project)));
    fs::copy(project.0.join("input.jsonl"), &legacy).unwrap();
    let arguments = ["--replay", "--repeat", "2", "--cassette-root", "legacy"];
    let good = sweep(&project, None, &arguments);
    assert!(
        good.status.success(),
        "{}",
        String::from_utf8_lossy(&good.stderr)
    );
    let index = project.0.join("sweep-results/plan.json");
    let report = project.0.join("results/sweep-comparison/comparison.json");
    let previous_index = fs::read(&index).unwrap();
    let previous_report = fs::read(&report).unwrap();
    let bundle = directory.join("recording.asb.zip");
    for kind in ["dangling", "directory"] {
        if kind == "dangling" {
            std::os::unix::fs::symlink(directory.join("absent"), &bundle).unwrap();
        } else {
            fs::create_dir(&bundle).unwrap();
        }
        let refused = sweep(&project, None, &arguments);
        assert!(
            !refused.status.success(),
            "{kind} silently selected legacy tape"
        );
        assert_eq!(fs::read(&index).unwrap(), previous_index);
        assert_eq!(fs::read(&report).unwrap(), previous_report);
        assert_eq!(
            records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
            2
        );
        if kind == "dangling" {
            fs::remove_file(&bundle).unwrap();
        } else {
            fs::remove_dir(&bundle).unwrap();
        }
    }
}

#[test]
fn repeated_legacy_default_keeps_each_replica_source_after_result_rotation() {
    let project = Project::new();
    project.config(2);
    fs::write(project.0.join("sweep.toml"), "configs=['benchmark.toml']\n").unwrap();
    let directory = project.0.join("sweep-results/000-benchmark/cassettes");
    fs::create_dir_all(&directory).unwrap();
    let source: Value =
        serde_json::from_slice(&fs::read(project.0.join("input.jsonl")).unwrap()).unwrap();
    for replica in 1..=2 {
        let mut interaction = source.clone();
        interaction["response"]["body"] = json!(json!({"recorded_track":100+replica}).to_string());
        fs::write(
            directory.join(format!(
                "{}-concurrency-{replica}.jsonl",
                legacy_name(&project)
            )),
            format!("{interaction}\n"),
        )
        .unwrap();
    }
    for arguments in [vec!["--replay", "--repeat", "2"], vec!["--replay"]] {
        let replayed = sweep(&project, None, &arguments);
        assert!(
            replayed.status.success(),
            "{}",
            String::from_utf8_lossy(&replayed.stderr)
        );
        let plan = campaign_plan(&replayed);
        let plan: Value = serde_json::from_slice(&fs::read(plan).unwrap()).unwrap();
        assert!(
            plan["replay_source"]
                .as_str()
                .unwrap()
                .contains("sweep-history")
        );
        assert!(plan["replay_selection"].is_null());
    }
    let attempts = records(&fs::read(project.0.join("attempts.jsonl")).unwrap());
    assert_eq!(attempts.len(), 6);
    for attempt in attempts {
        assert_eq!(
            attempt["response_tag"].as_u64(),
            attempt["replica"].as_u64().map(|replica| 100 + replica)
        );
    }
}

#[test]
fn independent_sweep_configurations_execute_concurrently_after_preparation() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    project.config(1);
    let template = fs::read_to_string(project.0.join("fixture-agent")).unwrap();
    let config = fs::read_to_string(project.0.join("benchmark.toml"))
        .unwrap()
        .replace("provider='fixture'", "provider='openrouter'");
    for name in ["agent-a", "agent-b"] {
        let script = template.replace(
            "root = pathlib.Path(__file__).parent\n",
            "root = pathlib.Path(__file__).parent\n(root / ('cross-' + pathlib.Path(__file__).name)).write_text('started')\ndeadline = time.monotonic() + 5\nwhile not all((root / ('cross-' + name)).exists() for name in ['agent-a', 'agent-b']):\n    assert time.monotonic() < deadline, 'independent sweep configurations were serialized'\n    time.sleep(.005)\n",
        );
        let agent = project.0.join(name);
        fixture_writer::write_file(&project.0, &agent, &script).unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
        let selected = config.replace(
            &project
                .0
                .join("fixture-agent")
                .to_string_lossy()
                .to_string(),
            &agent.to_string_lossy(),
        );
        fs::write(project.0.join(format!("{name}.toml")), selected).unwrap();
    }
    fs::write(
        project.0.join("sweep.toml"),
        "configs=['agent-a.toml','agent-b.toml']\n",
    )
    .unwrap();
    let provider = RecordingProvider::new();
    let result = sweep(
        &project,
        Some(&provider),
        &["--record", "--jobs", "2", "--max-active-attempts", "2"],
    );
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(project.0.join("cross-agent-a").exists());
    assert!(project.0.join("cross-agent-b").exists());
}
