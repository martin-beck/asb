//! Actual provider-free replay attempts through the owning sweep phases.
use super::*;
use crate::test_support as support;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use support::IsolatedEnvironment;

thread_local! {static FAIL_INITIAL: std::cell::Cell<bool> = const {std::cell::Cell::new(false)};}
thread_local! {static FAIL_COMPARE: std::cell::Cell<bool> = const {std::cell::Cell::new(false)};}
thread_local! {static ACTIVE: std::cell::Cell<bool> = const {std::cell::Cell::new(false)};}
thread_local! {static FAIL_FINAL: std::cell::Cell<bool> = const {std::cell::Cell::new(false)};}
thread_local! {static FAIL_PRICING: std::cell::Cell<bool> = const {std::cell::Cell::new(false)};}
thread_local! {static RESERVATION_FAULT: std::cell::Cell<Option<&'static str>> = const {std::cell::Cell::new(None)};}
pub(super) fn before_pricing_snapshot() -> Result<(), Error> {
    if FAIL_PRICING.get() {
        return Err(Error::Run("fixture pricing snapshot unavailable".into()));
    }
    Ok(())
}
pub(super) fn before_input_finish(results: &Path) {
    if FAIL_FINAL.get() {
        let input = results.join(".checkpoint/inputs.json");
        std::fs::rename(&input, results.join(".checkpoint/initial-inputs.saved")).unwrap();
        std::fs::create_dir(input).unwrap();
    }
}
pub(super) fn before_comparison(source: &Path) {
    if !ACTIVE.get() {
        return;
    }
    let parent = source.parent().unwrap();
    assert!(
        lock::OutputLock::acquire(parent, "sweep").is_err(),
        "comparison retains output ownership"
    );
    if FAIL_COMPARE.get() {
        std::fs::create_dir_all(parent.join("results")).unwrap();
        std::fs::write(
            parent.join("results/sweep-comparison"),
            "owned fixture report blocker",
        )
        .unwrap();
    }
}
pub(super) fn before_initial_seal(results: &Path) {
    if let Some(relative) = RESERVATION_FAULT.get() {
        let path = results.join(".checkpoint").join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "owned fixture reservation blocker").unwrap();
    }
    if FAIL_INITIAL.get() {
        std::fs::create_dir(results.join(".checkpoint")).unwrap();
        std::fs::write(results.join(".checkpoint/key"), "invalid owned fixture key").unwrap();
    }
}
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn read(path: impl AsRef<Path>) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
struct Writer {
    bytes: Vec<u8>,
    first: bool,
    fail: bool,
    failed: bool,
    after: usize,
}
impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.failed {
            self.after += 1;
            return Err(std::io::Error::other("writer was called after failure"));
        }
        let summary = self.bytes.windows(7).any(|w| w == b"\"task\":")
            || bytes.windows(7).any(|w| w == b"\"task\":");
        if self.fail && (self.first || (bytes.contains(&b'\n') && summary)) {
            self.failed = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "fixture terminal closed",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn actual_sweeps_stop_after_presentation_failure_but_settle_groups_and_partial_reports() {
    let Some(case) = std::env::var_os("ASB_SWEEP_PHASE_CASE") else {
        let executable = std::env::current_exe().unwrap();
        let runtime = executable.parent().unwrap().parent().unwrap().join("asb");
        assert!(runtime.is_file(), "build actual ASB before focused tests");
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/sweep-phase-fixtures")
            .join(crate::workloads::TaskIdentity::local("sweep-phase", None).attempt_id);
        for case in [
            "json",
            "replicas",
            "resume",
            "resume-mode",
            "history",
            "initial",
            "benchmark",
            "benchmark-output-compare",
            "json-final-seal",
            "campaign-initial",
            "campaign-final",
            "campaign-reservation-first",
            "campaign-reservation-last",
            "campaign-admission",
            "pricing-preflight",
            "pricing-replay-offline",
        ] {
            // The real capacity probe is intentionally exercised elsewhere.
            // This fixture has a separate assertion surface: continuation and
            // presentation failure handling.  Its child-only PATH supplies a
            // bounded `df -Pk` responder with sufficient free space.
            let fixture_bin = root.join("fixture-bin");
            std::fs::create_dir_all(&fixture_bin).unwrap();
            let fixture_df = fixture_bin.join("df");
            std::fs::write(
                &fixture_df,
                "#!/bin/sh\nprintf 'Filesystem 1024-blocks Used Available Capacity Mounted on\\nfixture 1 1 99999999 0%% /\\n'\n",
            )
            .unwrap();
            std::fs::set_permissions(&fixture_df, std::fs::Permissions::from_mode(0o700)).unwrap();
            let mut fixture_path = std::ffi::OsString::from(fixture_bin.as_os_str());
            fixture_path.push(":");
            fixture_path.push(std::env::var_os("PATH").unwrap());
            let mut command = Command::new(&executable);
            command
                .args(["--exact","sweep::phases::tests::actual_sweeps_stop_after_presentation_failure_but_settle_groups_and_partial_reports","--nocapture"])
                .isolated_env().env("PATH",fixture_path)
                .env("ASB_RUNTIME_EXECUTABLE",&runtime).env("ASB_SWEEP_PHASE_CASE",case);
            if case == "pricing-replay-offline" {
                command.env("OPENROUTER_API_KEY", "fixture-key");
            }
            let child = command.output().unwrap();
            assert!(
                child.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&child.stdout),
                String::from_utf8_lossy(&child.stderr)
            );
        }
        return;
    };
    let case = case.to_str().unwrap();
    ACTIVE.set(true);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/sweep-phase-fixtures")
        .join(crate::workloads::TaskIdentity::local("sweep-phase", None).attempt_id);
    std::fs::create_dir_all(&root).unwrap();
    let fixture = Fixture(root.canonicalize().unwrap());
    let root = &fixture.0;
    std::fs::create_dir(root.join(".asb")).unwrap();
    let project = crate::project::ProjectConfig {
        tools_dir: "tools".into(),
        workloads_dir: "workloads".into(),
        workspace_dir: "workspace".into(),
        ..Default::default()
    };
    std::fs::write(
        root.join(".asb/project.toml"),
        toml::to_string(&project).unwrap(),
    )
    .unwrap();
    std::fs::create_dir(root.join("workloads")).unwrap();
    crate::workloads::populate(&root.join("workloads")).unwrap();
    let node = crate::tools::host_executable(Path::new("node")).unwrap();
    let counter = root.join("calls");
    let counter_js = serde_json::to_string(&counter).unwrap();
    let agent = root.join("tools/pi/bin/pi");
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    let body = json!({"model":"fixture","tools":[{"type":"function","function":{"name":"fixture","parameters":{"type":"object"}}}],"messages":[]});
    let response = json!({"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"text","text":"src/cli.py src/registry.py lookup"}]}]});
    std::fs::write(&agent,format!("#!{}\nconst fs=require('node:fs');fs.appendFileSync({counter_js},process.cwd()+'\\n');const body={body};body.model=process.env.ASB_MODEL;fetch(process.env.OPENAI_BASE_URL+'/chat/completions',{{method:'POST',headers:{{'content-type':'application/json'}},body:JSON.stringify(body)}}).then(async r=>{{if(!r.ok)throw new Error('fixture request');await r.text();fs.writeFileSync(process.env.ASB_PI_SUMMARY_PATH,JSON.stringify({{schema_version:1,completed:true,steps:1,usage_complete:false,final_answer:'src/cli.py src/registry.py lookup'}}),{{mode:0o600}});console.log(JSON.stringify({response}));process.exitCode=process.env.ASB_MODEL==='bad'?7:0;}}).catch(()=>{{process.exitCode=9;}});\n",node.display())).unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let inputs = root.join("input");
    let replicas = if case == "replicas" { 2 } else { 1 };
    for (index, name) in ["a", "b"].into_iter().enumerate() {
        let model = if case.starts_with("benchmark") && index == 0 {
            "bad"
        } else {
            "fixture"
        };
        let n = if index == 0 { replicas } else { 1 };
        std::fs::write(root.join(format!("{name}.toml")),format!("agent='pi'\nprovider='openrouter'\nmodel='{model}'\nworkload='repository-navigation'\nconcurrency={n}\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=[]\n")).unwrap();
        let cassette = inputs.join(format!(
            "{index:03}-{name}/cassettes/benchmark-pi-repository-navigation-{model}.jsonl"
        ));
        std::fs::create_dir_all(cassette.parent().unwrap()).unwrap();
        let mut body = body.clone();
        body["model"] = json!(model);
        let row = json!({"request":{"method":"POST","url":"/v1/chat/completions","body":body.to_string()},"response":{"status":200,"headers":{"content-type":"application/json"},"body":"{}"}});
        std::fs::write(cassette, format!("{row}\n")).unwrap();
    }
    let manifest = root.join("sweep.toml");
    std::fs::write(&manifest, "configs=['a.toml','b.toml']\n").unwrap();
    if case == "history" || case == "pricing-preflight" {
        std::fs::create_dir(root.join("sweep-results")).unwrap();
        std::fs::write(root.join("sweep-results/old-sentinel"), "preserved").unwrap();
    }
    let options = Options {
        json: case != "history",
        mode: Some(crate::tape::Mode::Replay),
        cassette_root: Some(&inputs),
        ..Default::default()
    };
    if case.starts_with("campaign-") {
        FAIL_INITIAL.set(case == "campaign-initial");
        FAIL_FINAL.set(case == "campaign-final");
        RESERVATION_FAULT.set(match case {
            "campaign-reservation-first" => Some("campaign-reservations/000000.json"),
            "campaign-reservation-last" => Some("campaign-reservations/000001.json"),
            "campaign-admission" => Some("campaign-admission.json"),
            _ => None,
        });
        let mut output = Vec::new();
        run_with_options(
            manifest.to_str().unwrap(),
            Options {
                repeat: Some(2),
                ..options
            },
            &mut output,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().lines().count(),
            4
        );
        let rows = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let code = if case == "campaign-final" {
            "input_seal_unavailable"
        } else {
            "initial_input_seal_unavailable"
        };
        assert_eq!(
            rows.iter()
                .filter(|row| row["event"] == "continuation_warning" && row["code"] == code)
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row["workload_success"] == true)
                .count(),
            4
        );
        let comparison = read(root.join("results/sweep-comparison/comparison.json"));
        assert_eq!(comparison["benchmarks"].as_array().unwrap().len(), 4);
        if case == "campaign-initial" {
            assert_eq!(
                std::fs::read_to_string(root.join("sweep-results/.checkpoint/key")).unwrap(),
                "invalid owned fixture key"
            );
            assert!(!root.join("sweep-results/.checkpoint/inputs.json").exists());
        } else if case == "campaign-final" {
            assert!(root.join("sweep-results/.checkpoint/inputs.json").is_dir());
            assert_eq!(
                read(root.join("sweep-results/.checkpoint/initial-inputs.saved"))["status"],
                "pending"
            );
        } else {
            let checkpoint = root.join("sweep-results/.checkpoint");
            assert_eq!(read(checkpoint.join("inputs.json"))["status"], "pending");
            assert_eq!(
                std::fs::read_to_string(checkpoint.join(RESERVATION_FAULT.get().unwrap())).unwrap(),
                "owned fixture reservation blocker"
            );
            if case != "campaign-admission" {
                assert!(!checkpoint.join("campaign-admission.json").exists());
            }
        }
        assert!(lock::OutputLock::acquire(root, "sweep").is_ok());
        return;
    }
    if case == "resume" || case == "resume-mode" {
        run_with_options(manifest.to_str().unwrap(), options, &mut Vec::new()).unwrap();
        std::fs::write(
            root.join("sweep-results/.terminal-output-000001.tmp"),
            "unadmitted interrupted output sentinel",
        )
        .unwrap();
    }
    FAIL_INITIAL.set(case == "initial");
    FAIL_COMPARE.set(case == "benchmark-output-compare");
    FAIL_FINAL.set(case == "json-final-seal");
    FAIL_PRICING.set(case == "pricing-preflight" || case == "pricing-replay-offline");
    let mut writer = Writer {
        bytes: Vec::new(),
        first: case == "initial" || case == "history",
        fail: case != "benchmark" && case != "resume-mode" && case != "pricing-replay-offline",
        failed: false,
        after: 0,
    };
    let outcome = run_with_options(
        manifest.to_str().unwrap(),
        Options {
            resume: case == "resume" || case == "resume-mode",
            mode: if case == "resume-mode" || case == "pricing-preflight" {
                None
            } else {
                options.mode
            },
            cassette_root: if case == "pricing-preflight" {
                None
            } else {
                options.cassette_root
            },
            ..options
        },
        &mut writer,
    );
    assert!(lock::OutputLock::acquire(root, "sweep").is_ok());
    if case == "pricing-replay-offline" {
        outcome.unwrap();
        assert_eq!(
            std::fs::read_to_string(&counter).unwrap().lines().count(),
            2
        );
        assert!(root.join("results/sweep-comparison/report.html").is_file());
        return;
    }
    assert!(outcome.is_err(), "{case}");
    if case == "pricing-preflight" {
        assert!(
            outcome
                .unwrap_err()
                .to_string()
                .contains("fixture pricing snapshot unavailable")
        );
        assert_eq!(
            std::fs::read_to_string(root.join("sweep-results/old-sentinel")).unwrap(),
            "preserved"
        );
        assert!(!root.join("sweep-history").exists());
        assert!(!counter.exists());
        return;
    }
    let diagnostic = outcome.as_ref().err().map(ToString::to_string);
    if case == "resume-mode" {
        assert!(
            outcome
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("cannot resume")
        );
        assert!(writer.bytes.is_empty());
        assert_eq!(
            read(root.join("sweep-results/plan.json"))["execution_mode"],
            "replay"
        );
    }
    if case != "benchmark" && case != "resume-mode" {
        let error = outcome.unwrap_err().to_string();
        assert!(error.contains("fixture terminal closed"));
        if case == "benchmark-output-compare" {
            assert!(error.starts_with("sweep completed with 1 failed benchmark"));
            assert!(error.contains("comparison failed"));
        }
        if case == "json-final-seal" {
            assert!(error.contains("final input-stability seal unavailable"));
        }
        assert!(writer.failed);
    }
    assert_eq!(writer.after, 0, "never retry a latched writer");
    if case == "resume" || case == "resume-mode" {
        assert_eq!(
            std::fs::read_to_string(root.join("sweep-results/.terminal-output-000001.tmp"))
                .unwrap(),
            "unadmitted interrupted output sentinel"
        );
    }
    let calls = std::fs::read_to_string(&counter).unwrap_or_default();
    let expected = match case {
        "history" | "initial" => 0,
        "replicas" | "resume" | "resume-mode" | "benchmark" => 2,
        _ => 1,
    };
    if calls.lines().count() != expected {
        // Fixture-only diagnostics: never print run output, paths or prompts.
        // Read a bounded prefix and classify it so a prelaunch failure remains
        // distinguishable from a presentation or provider failure in CI.
        use std::io::Read;
        let mut bytes = writer
            .bytes
            .iter()
            .copied()
            .take(65_536)
            .collect::<Vec<_>>();
        let mut readable = Vec::new();
        for path in [
            "sweep-results/000-a/run.log",
            "results/sweep-comparison/comparison.json",
        ] {
            let mut prefix = Vec::new();
            readable.push(
                std::fs::File::open(root.join(path))
                    .and_then(|file| file.take(65_536).read_to_end(&mut prefix))
                    .is_ok(),
            );
            bytes.extend(prefix);
        }
        let text = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
        let indicators = [
            "gate",
            "spawn",
            "permission",
            "timeout",
            "provider",
            "replay",
            "cassette",
        ]
        .into_iter()
        .filter(|word| text.contains(word))
        .collect::<Vec<_>>();
        panic!(
            "{case}: expected {expected} fixture launches, observed {}; run_and_comparison_readable={readable:?} inspected_bytes={} indicators={indicators:?}",
            calls.lines().count(),
            bytes.len()
        );
    }
    if matches!(case, "json" | "replicas") {
        assert!(!root.join("sweep-results/001-b").exists());
    }
    if case != "benchmark-output-compare" {
        assert!(
            root.join("results/sweep-comparison/report.html").is_file(),
            "{case}: {diagnostic:?}"
        );
        let comparison = read(root.join("results/sweep-comparison/comparison.json"));
        assert!(!comparison["benchmarks"].as_array().unwrap().is_empty());
    }
    if case == "json" {
        // Actual stopped-output run: measurements must remain self-contained,
        // while old omissions can borrow only the agreeing original outcome.
        let measurement = root.join("sweep-results/000-a/measurements/run.json");
        let original = std::fs::read(&measurement).unwrap();
        let measured: Value = serde_json::from_slice(&original).unwrap();
        let summary = read(root.join("sweep-results/000-a/run.log"));
        assert!(measured["planning"].is_object());
        assert!(measured["effective_budgets"].is_object());
        assert_eq!(measured["planning"], summary["planning"]);
        assert_eq!(measured["effective_budgets"], summary["effective_budgets"]);
        let mut omitted = measured.clone();
        omitted.as_object_mut().unwrap().remove("planning");
        omitted.as_object_mut().unwrap().remove("effective_budgets");
        assert!(crate::sweep::checkpoint::completed::measurement_matches(
            &omitted, &summary
        ));
        std::fs::write(&measurement, serde_json::to_vec(&omitted).unwrap()).unwrap();
        crate::compare::generate(manifest.to_str().unwrap(), &mut Vec::new()).unwrap();
        let report = root.join("results/sweep-comparison/comparison.json");
        let prior = std::fs::read(&report).unwrap();
        for field in ["planning", "effective_budgets"] {
            let mut contradictory = omitted.clone();
            contradictory[field] = json!({"wrong":"cannot replace original policy"});
            assert!(!crate::sweep::checkpoint::completed::measurement_matches(
                &contradictory,
                &summary
            ));
            std::fs::write(&measurement, serde_json::to_vec(&contradictory).unwrap()).unwrap();
            assert!(
                crate::compare::generate(manifest.to_str().unwrap(), &mut Vec::new())
                    .unwrap_err()
                    .to_string()
                    .contains("disagree on recorded")
            );
            assert_eq!(std::fs::read(&report).unwrap(), prior);
        }
        std::fs::write(&measurement, original).unwrap();
    }
    if case != "initial" && case != "json-final-seal" {
        assert_eq!(
            read(root.join("sweep-results/.checkpoint/inputs.json"))["status"],
            "stable"
        );
    }
    if case == "history" {
        let plan = read(root.join("sweep-results/plan.json"));
        assert_eq!(
            std::fs::read_to_string(
                root.join("sweep-history")
                    .join(plan["run_id"].as_str().unwrap())
                    .join("sweep-results/old-sentinel")
            )
            .unwrap(),
            "preserved"
        );
    }
    if matches!(
        case,
        "json" | "replicas" | "benchmark" | "benchmark-output-compare"
    ) {
        let dir = root.join("sweep-results/000-a");
        let records: Vec<_> = std::fs::read_dir(dir.join(".asb-completions"))
            .unwrap()
            .collect();
        assert_eq!(records.len(), replicas);
        for record in records {
            let record = read(record.unwrap().path());
            assert_eq!(record["summary"]["workload_success"], true);
            assert_eq!(
                record["summary"]["benchmark_outcome"]["cleanup_confirmed"],
                true
            );
            assert!(
                record["summary"]["execution_plan_fingerprint"]
                    .as_str()
                    .unwrap()
                    .starts_with("asb-protocol-v1:")
            );
        }
        assert!(
            root.join("sweep-results/.checkpoint/completed/000000.json")
                .is_file()
        );
    }
}
