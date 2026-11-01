//! Provider-free real CLI qualification of fresh repeated execution.
#![cfg(unix)]
#[path = "support/fixture_namespace.rs"]
mod fixture_namespace;
#[cfg(unix)]
#[path = "support/fixture_writer.rs"]
mod fixture_writer;
mod support;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use support::IsolatedEnvironment;

#[path = "support/repeated_sweep.rs"]
mod sweep_tests;

/// Keep Cargo's already-installed toolchain available to a fixture whose
/// private HOME deliberately excludes all ambient application state. Rustup
/// otherwise treats that private HOME as a new installation and may try to
/// bootstrap a toolchain while a workload only needs to probe `cargo`.
fn retain_host_rust_toolchain(command: &mut Command) -> &mut Command {
    let host_home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .unwrap_or_else(|| PathBuf::from(&host_home).join(".rustup").into_os_string());
    let cargo_home = std::env::var_os("CARGO_HOME")
        .unwrap_or_else(|| PathBuf::from(&host_home).join(".cargo").into_os_string());
    command
        .env("RUSTUP_HOME", rustup_home)
        .env("CARGO_HOME", cargo_home)
}

struct Project(PathBuf);
impl Project {
    fn new() -> Self {
        let path = fixture_namespace::allocate(&std::env::temp_dir(), "asb-repeated-").unwrap();
        fs::create_dir_all(path.join(".asb")).unwrap();
        fs::write(
            path.join(".asb/project.toml"),
            "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
        )
        .unwrap();
        fs::create_dir(path.join("workloads")).unwrap();
        fs::write(
            path.join("workloads/bug-fix.txt"),
            "Repair src/parser.py so parse_path ignores empty components.",
        )
        .unwrap();
        fs::create_dir(path.join("workspace")).unwrap();
        fs::write(
            path.join("workspace/preserve-user-file"),
            "untouched user material",
        )
        .unwrap();
        let body = json!({"model":"fixture","tools":[{"type":"function","function":{"name":"fixture","parameters":{"type":"object"}}}],"messages":[]});
        fs::write(path.join("input.jsonl"), format!("{}\n",json!({"request":{"method":"POST","url":"/v1/chat/completions","body":body.to_string()},"response":{"status":200,"headers":{"content-type":"application/json"},"body":"{}"}}))).unwrap();
        let script = path.join("fixture-agent");
        fixture_writer::write_file(&path, &script, r##"#!/usr/bin/env python3
import json, os, pathlib, sys, time, urllib.request
root = pathlib.Path(__file__).parent
workspace = pathlib.Path.cwd()
batch = int(workspace.parent.name.split('-')[-1])
replica = int(workspace.name.split('-')[-1]) if '-concurrency-' in workspace.name else 1
before = (workspace/'src/parser.py').read_text()
runtime = pathlib.Path(str(workspace)+'.asb-agent-runtime')
runtime.mkdir(exist_ok=True)
(runtime/'session').write_text(str(os.getpid()))
start = time.monotonic_ns()
(root/f'barrier-{batch}-{replica}').write_text('ready')
expected = int((root/'expected-concurrency').read_text())
deadline = time.monotonic()+5
while not all((root/f'barrier-{batch}-{index}').exists() for index in range(1,expected+1)):
    assert time.monotonic()<deadline, 'replicas did not overlap'
    time.sleep(.005)
if (root/'reverse-arrivals').exists() and replica == 1: time.sleep(.04)
body = {'model':'fixture','tools':[{'type':'function','function':{'name':'fixture','parameters':{'type':'object'}}}],'messages':[]}
url = os.environ['ASB_PROVIDER_BASE_URL'].rstrip('/')
if url.endswith('/v1'): url = url[:-3]
with urllib.request.urlopen(urllib.request.Request(url+'/v1/chat/completions',data=json.dumps(body).encode(),headers={'Content-Type':'application/json'}),timeout=5) as response:
    assert response.status == 200
    response_tag = json.load(response).get('recorded_track')
time.sleep(.06)
if batch == 2 and replica == 1 and (root/'block-union-ledger').exists():
    (workspace.parent.parent/'recording-current.json').mkdir()
(workspace/'src/parser.py').write_text("def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n")
row = json.dumps({'batch':batch,'replica':replica,'workspace':str(workspace),'before':before,'runtime':str(runtime),'start':start,'end':time.monotonic_ns(),'response_tag':response_tag})+'\n'
fd = os.open(root/'attempts.jsonl',os.O_WRONLY|os.O_CREAT|os.O_APPEND,0o600)
os.write(fd,row.encode()); os.close(fd)
if batch == 1 and replica == 1:
    if (root/'noisy-record').exists():
        sys.stdout.buffer.write(b'x' * (8*1024*1024+1024)); sys.stdout.buffer.flush()
    if (root/'block-bundle-commit').exists(): (root/'.new-recording.jsonl.partial/track-1.jsonl').write_text('preserved blocking stage')
    if (root/'block-outcome-first').exists(): (workspace.parent/'.asb-completions').write_text('retain this blocking fixture')
    if (root/'grader-error-first').exists(): (workspace/'src/parser.py').write_bytes(bytes([255]))
    if (root/'false-grade-first').exists(): (workspace/'src/parser.py').write_text(before)
    if (root/'mutate-source').exists(): (root/'input.jsonl').write_text('changed after planning')
    if (root/'replace-bundle').exists(): (root/'new-recording.jsonl').write_text('replaced after private staging')
    if (root/'mutate-prompt').exists(): (root/'workloads/bug-fix.txt').write_text('Changed instructions must not launch another batch.')
    if (root/'capture-overflow-first').exists(): sys.stdout.buffer.write(b'x'*(9*1024*1024)); sys.stdout.buffer.flush()
sys.exit(7 if (root/'fail-first').exists() and batch == 1 and replica == 1 else 0)
"##).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn config(&self, concurrency: usize) {
        fs::write(self.0.join("expected-concurrency"), concurrency.to_string()).unwrap();
        fs::write(self.0.join("benchmark.toml"),format!("agent='{}'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\nworkspace='workspace'\nlog_file='original.log'\nconcurrency={concurrency}\n",self.0.join("fixture-agent").display())).unwrap();
    }
    fn run(&self, repeat: &str) -> Output {
        self.run_extra(repeat, &[])
    }
    fn run_extra(&self, repeat: &str, extra: &[&str]) -> Output {
        // This fixture intentionally gives the child a private HOME.  The
        // bundled workload preflight probes Cargo, whose rustup shim otherwise
        // interprets that private HOME as a fresh installation and starts a
        // network toolchain bootstrap.  Preserve only the already-installed
        // Rust homes from the test runner; all application state remains
        // isolated by `isolated_env` below.
        let mut command = Command::new(env!("CARGO_BIN_EXE_asb"));
        command.current_dir(&self.0).isolated_env();
        retain_host_rust_toolchain(&mut command);
        command
            .env("HOME", &self.0)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .args([
                "run",
                "benchmark.toml",
                "--replay",
                "--cassette",
                "input.jsonl",
                "--repeat",
                repeat,
                "--json",
            ])
            .args(extra)
            .output()
            .unwrap()
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = fixture_writer::cleanup(&self.0);
    }
}
fn records(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn campaign_plan(output: &Output) -> PathBuf {
    PathBuf::from(
        records(&output.stdout)
            .iter()
            .find(|row| row["type"] == "execution-plan")
            .unwrap()["path"]
            .as_str()
            .unwrap(),
    )
}

fn compare_campaign(project: &Project, plan: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_asb"))
        .current_dir(&project.0)
        .isolated_env()
        .env("HOME", &project.0)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .arg("compare")
        .arg(plan)
        .output()
        .unwrap()
}

#[test]
fn campaign_comparison_enumerates_every_batch_and_preserves_failed_attempt_grades() {
    let project = Project::new();
    project.config(2);
    fs::write(project.0.join("fail-first"), "one ordinary process failure").unwrap();
    let run = project.run("3");
    assert!(!run.status.success());
    let plan = campaign_plan(&run);
    let mut cohorts = std::collections::BTreeMap::new();
    for summary in records(&run.stdout)
        .into_iter()
        .filter(|row| row["workload_success"].is_boolean())
    {
        let execution = &summary["execution"];
        let batch = execution["batch"].as_u64().unwrap();
        let replica = execution["replica"].as_u64().unwrap();
        let start: Value = serde_json::from_slice(
            &fs::read(
                plan.parent()
                    .unwrap()
                    .join(format!("batch-{batch}/starts/{replica}.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(start["execution"], *execution);
        assert_eq!(start["execution_cohort"], summary["execution_cohort"]);
        assert_eq!(start["execution_cohort"]["selected_replicas"], 2);
        assert_eq!(start["execution_cohort"]["admitted_slots"], json!([1, 2]));
        let id = start["execution_cohort"]["id"].as_str().unwrap().to_owned();
        if let Some(previous) = cohorts.insert(batch, id.clone()) {
            assert_eq!(previous, id);
        }
    }
    assert_eq!(cohorts.len(), 3);
    assert_eq!(
        cohorts
            .values()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
    let output = compare_campaign(&project, &plan);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let directory = plan.parent().unwrap().join("comparison");
    let report: Value =
        serde_json::from_slice(&fs::read(directory.join("comparison.json")).unwrap()).unwrap();
    assert_eq!(report["campaign"]["repeat"], 3);
    assert_eq!(report["campaign"]["execution_mode"], "replay");
    let rows = report["benchmarks"].as_array().unwrap();
    assert_eq!(rows.len(), 6);
    let mut attempts = std::collections::BTreeSet::new();
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row["id"]["batch"], index / 2 + 1);
        assert_eq!(row["id"]["replica"], index % 2 + 1);
        assert_eq!(row["execution"]["slot"], index + 1);
        assert_eq!(row["metrics"]["success_rate"], 1.0);
        assert!(attempts.insert(row["task"]["attempt_id"].as_str().unwrap()));
    }
    assert!(directory.join("report.html").is_file());
    assert!(!report["graphs"].as_array().unwrap().is_empty());
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        6
    );
}

#[test]
fn campaign_comparison_rejects_present_invalid_references_without_replacing_previous_report() {
    let project = Project::new();
    project.config(1);
    let run = project.run("2");
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let plan = campaign_plan(&run);
    assert!(compare_campaign(&project, &plan).status.success());
    let root = plan.parent().unwrap();
    let report = root.join("comparison/comparison.json");
    let previous = fs::read(&report).unwrap();
    let reference = root.join("batch-1/outcomes/1.json");
    let original = fs::read(&reference).unwrap();
    for field in ["execution", "path", "sha256"] {
        let mut value: Value = serde_json::from_slice(&original).unwrap();
        match field {
            "execution" => value[field]["batch"] = json!(2),
            "path" => value[field] = json!("batch-2/.asb-completions/foreign.json"),
            _ => value[field] = json!("0".repeat(64)),
        }
        fs::write(&reference, serde_json::to_vec(&value).unwrap()).unwrap();
        let compared = compare_campaign(&project, &plan);
        assert!(!compared.status.success(), "{field}");
        assert_eq!(fs::read(&report).unwrap(), previous);
    }
    fs::write(&reference, original).unwrap();
    assert!(compare_campaign(&project, &plan).status.success());
    fs::remove_file(&reference).unwrap();
    assert!(
        compare_campaign(&project, &plan).status.success(),
        "honest log fallback"
    );
    fs::remove_file(root.join("batch-1/run.jsonl")).unwrap();
    assert!(
        compare_campaign(&project, &plan).status.success(),
        "missing slot remains visible"
    );
    let value: Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
    assert_eq!(value["benchmarks"].as_array().unwrap().len(), 2);
    assert_eq!(value["benchmarks"][0]["result_source"], "missing");
    assert!(value["benchmarks"][0]["execution"].is_null());
}

#[test]
fn campaign_comparison_binds_measurement_only_rows_to_exact_execution() {
    let project = Project::new();
    project.config(1);
    let run = project.run_extra("2", &["--measurements", "captures"]);
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let plan_path = campaign_plan(&run);
    let root = plan_path.parent().unwrap();
    let plan: Value = serde_json::from_slice(&fs::read(&plan_path).unwrap()).unwrap();
    let measures = PathBuf::from(plan["measurement_root"].as_str().unwrap());
    for batch in 1..=2 {
        let measured: Value = serde_json::from_slice(
            &fs::read(measures.join(format!("batch-{batch}/measurements/run.json"))).unwrap(),
        )
        .unwrap();
        assert!(measured["planning"].is_object());
        assert!(measured["effective_budgets"].is_object());
        fs::remove_file(root.join(format!("batch-{batch}/outcomes/1.json"))).unwrap();
        fs::remove_file(root.join(format!("batch-{batch}/run.jsonl"))).unwrap();
    }
    let good = compare_campaign(&project, &plan_path);
    assert!(
        good.status.success(),
        "{}",
        String::from_utf8_lossy(&good.stderr)
    );
    let report = root.join("comparison/comparison.json");
    let previous = fs::read(&report).unwrap();
    let source = measures.join("batch-1/measurements/run.json");
    let original = fs::read(&source).unwrap();
    for field in ["campaign", "batch", "slot", "replica", "total"] {
        let mut value: Value = serde_json::from_slice(&original).unwrap();
        value["execution"][field] = if field == "campaign" {
            json!("foreign")
        } else {
            json!(99)
        };
        fs::write(&source, serde_json::to_vec(&value).unwrap()).unwrap();
        let result = compare_campaign(&project, &plan_path);
        assert!(!result.status.success(), "accepted foreign {field}");
        assert_eq!(fs::read(&report).unwrap(), previous);
    }
    for field in ["planning", "effective_budgets"] {
        let mut value: Value = serde_json::from_slice(&original).unwrap();
        value.as_object_mut().unwrap().remove(field);
        fs::write(&source, serde_json::to_vec(&value).unwrap()).unwrap();
        let result = compare_campaign(&project, &plan_path);
        assert!(
            !result.status.success(),
            "measurement-only dispatch must not guess absent {field}"
        );
        assert_eq!(fs::read(&report).unwrap(), previous);
    }
    fs::write(source, original).unwrap();
    assert!(compare_campaign(&project, &plan_path).status.success());
}

#[test]
fn campaign_comparison_rejects_excessive_primary_observations_before_decoding_payloads() {
    let project = Project::new();
    project.config(1);
    let run = project.run("17");
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let plan = campaign_plan(&run);
    let root = plan.parent().unwrap();
    for batch in 1..=17 {
        fs::File::create(root.join(format!("batch-{batch}/run.jsonl")))
            .unwrap()
            .set_len(16 * 1024 * 1024)
            .unwrap();
    }
    let compared = compare_campaign(&project, &plan);
    assert!(!compared.status.success());
    assert!(
        String::from_utf8_lossy(&compared.stderr).contains("aggregate primary observations exceed"),
        "{}",
        String::from_utf8_lossy(&compared.stderr)
    );
    assert!(!root.join("comparison").exists());
}

#[test]
fn campaign_comparison_rejects_recording_disagreement_with_measurements() {
    let project = Project::new();
    project.config(1);
    let config_path = project.0.join("benchmark.toml");
    let configuration = fs::read_to_string(&config_path).unwrap();
    fs::write(config_path, format!("{configuration}\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=[]\n")).unwrap();
    let provider = RecordingProvider::new();
    let run = record(&project, &provider, Some("2"));
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let plan = campaign_plan(&run);
    let root = plan.parent().unwrap();
    let good = compare_campaign(&project, &plan);
    assert!(
        good.status.success(),
        "{}",
        String::from_utf8_lossy(&good.stderr)
    );
    let report = root.join("comparison/comparison.json");
    let previous = fs::read(&report).unwrap();
    let measurement = root.join("batch-1/measurements/run.json");
    let original = fs::read(&measurement).unwrap();
    let recorded: Value = serde_json::from_slice(&original).unwrap();
    assert!(recorded["recording"].is_object());
    for variant in ["missing", "null", "changed"] {
        let mut value = recorded.clone();
        match variant {
            "missing" => {
                value.as_object_mut().unwrap().remove("recording");
            }
            "null" => value["recording"] = Value::Null,
            _ => value["recording"]["attempt_id"] = json!("foreign"),
        }
        fs::write(&measurement, serde_json::to_vec(&value).unwrap()).unwrap();
        let compared = compare_campaign(&project, &plan);
        assert!(!compared.status.success(), "{variant}");
        assert_eq!(fs::read(&report).unwrap(), previous);
    }
    fs::write(measurement, original).unwrap();
    assert!(compare_campaign(&project, &plan).status.success());
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn campaign_comparison_rejects_intermediate_symlink_aliases() {
    let project = Project::new();
    project.config(1);
    let run = project.run("1");
    assert!(run.status.success());
    let plan = campaign_plan(&run);
    assert!(compare_campaign(&project, &plan).status.success());
    let root = plan.parent().unwrap();
    let report = root.join("comparison/comparison.json");
    let previous = fs::read(&report).unwrap();
    for directory in ["outcomes", ".asb-completions"] {
        let path = root.join("batch-1").join(directory);
        let held = root.join("batch-1").join(format!("{directory}-held"));
        fs::rename(&path, &held).unwrap();
        std::os::unix::fs::symlink(&held, &path).unwrap();
        let compared = compare_campaign(&project, &plan);
        assert!(!compared.status.success(), "{directory}");
        assert_eq!(fs::read(&report).unwrap(), previous);
        fs::remove_file(&path).unwrap();
        fs::rename(held, path).unwrap();
    }
    assert!(compare_campaign(&project, &plan).status.success());
}

#[path = "support/recording_provider.rs"]
mod recording_provider;
use recording_provider::RecordingProvider;
fn record(project: &Project, provider: &RecordingProvider, repeat: Option<&str>) -> Output {
    record_to(project, provider, repeat, Some("new-recording.jsonl"))
}

fn record_to(
    project: &Project,
    provider: &RecordingProvider,
    repeat: Option<&str>,
    cassette: Option<&str>,
) -> Output {
    record_to_with_mode(project, provider, repeat, cassette, "--record")
}

fn record_to_with_mode(
    project: &Project,
    provider: &RecordingProvider,
    repeat: Option<&str>,
    cassette: Option<&str>,
    mode: &str,
) -> Output {
    let config = project.0.join("benchmark.toml");
    fs::write(
        &config,
        fs::read_to_string(&config)
            .unwrap()
            .replace("provider='fixture'", "provider='openrouter'"),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_asb"));
    command
        .current_dir(&project.0)
        .isolated_env()
        .env("HOME", &project.0)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("ASB_PROVIDER_BASE_URL", &provider.endpoint)
        .env("OPENROUTER_API_KEY", "fixture-only-key")
        .env("OPENROUTER_BASE_URL", format!("{}/v1", provider.endpoint))
        .env(
            "OPENROUTER_MODELS_URL",
            format!("{}/models", provider.endpoint),
        )
        .args(["run", "benchmark.toml", mode, "--json"]);
    if let Some(cassette) = cassette {
        command.args(["--cassette", cassette]);
    }
    if let Some(repeat) = repeat {
        command.args(["--repeat", repeat]);
    }
    command.output().unwrap()
}

fn replay_bundle(project: &Project, extra: &[&str]) -> Output {
    replay_from(project, Some("new-recording.jsonl"), extra)
}

fn replay_from(project: &Project, cassette: Option<&str>, extra: &[&str]) -> Output {
    replay_with_format(project, cassette, extra, true)
}

fn replay_with_format(
    project: &Project,
    cassette: Option<&str>,
    extra: &[&str],
    json: bool,
) -> Output {
    replay_using_mode(project, cassette, extra, json, "--replay")
}

fn replay_using_mode(
    project: &Project,
    cassette: Option<&str>,
    extra: &[&str],
    json: bool,
    mode: &str,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_asb"));
    command
        .current_dir(&project.0)
        .isolated_env()
        .env("HOME", &project.0)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("OPENROUTER_BASE_URL", "http://127.0.0.1:1")
        .args(["run", "benchmark.toml", mode]);
    if json {
        command.arg("--json");
    }
    if let Some(cassette) = cassette {
        command.args(["--cassette", cassette]);
    }
    command.args(extra).output().unwrap()
}

#[test]
fn workload_output_mentioning_refusal_does_not_fail_successful_agent() {
    let project = Project::new();
    project.config(1);
    let agent = project.0.join("fixture-agent");
    let script = fs::read_to_string(&agent).unwrap().replace(
        "sys.exit(7 if (root/'fail-first').exists()",
        "print('workload test: cannot inspect a nonexistent fixture file')\nsys.exit(7 if (root/'fail-first').exists()",
    );
    fs::write(&agent, script).unwrap();
    let result = project.run("1");
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn workload_output_mentioning_provider_error_does_not_fail_successful_agent() {
    let project = Project::new();
    project.config(1);
    let agent = project.0.join("fixture-agent");
    let script = fs::read_to_string(&agent).unwrap().replace(
        "sys.exit(7 if (root/'fail-first').exists()",
        "print('workload output: litellm.NotFoundError is a documented example')\nsys.exit(7 if (root/'fail-first').exists()",
    );
    fs::write(&agent, script).unwrap();
    let result = project.run("1");
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn llm_only_record_and_replay_do_not_require_tool_capture() {
    let project = Project::new();
    project.config(1);
    let provider = RecordingProvider::new();
    let recorded = record_to_with_mode(
        &project,
        &provider,
        None,
        Some("llm-only.zip"),
        "--llm-record",
    );
    assert!(
        recorded.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&recorded.stdout),
        String::from_utf8_lossy(&recorded.stderr)
    );
    assert!(project.0.join("llm-only.zip").is_file());
    drop(provider);
    let replayed = replay_using_mode(&project, Some("llm-only.zip"), &[], true, "--llm-replay");
    assert!(
        replayed.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&replayed.stdout),
        String::from_utf8_lossy(&replayed.stderr)
    );
}

#[test]
fn default_record_and_replay_pair_while_existing_legacy_default_keeps_precedence() {
    let project = Project::new();
    project.config(1);
    let provider = RecordingProvider::new();
    let recorded = record_to(&project, &provider, None, None);
    assert!(
        recorded.status.success(),
        "{}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    drop(provider);
    let bundles: Vec<_> = fs::read_dir(project.0.join("cassettes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "zip"))
        .collect();
    assert_eq!(bundles.len(), 1);
    let replayed = replay_from(&project, None, &[]);
    assert!(
        replayed.status.success(),
        "{}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    assert!(
        records(&replayed.stdout)
            .iter()
            .any(|row| row["type"] == "replay-selection")
    );
    let name = bundles[0]
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .strip_suffix(".asb.zip")
        .unwrap();
    let legacy = bundles[0].with_file_name(format!("{name}.jsonl"));
    fs::copy(project.0.join("input.jsonl"), &legacy).unwrap();
    let legacy_replay = replay_from(&project, None, &[]);
    assert!(
        legacy_replay.status.success(),
        "{}",
        String::from_utf8_lossy(&legacy_replay.stderr)
    );
    assert!(
        records(&legacy_replay.stdout)
            .iter()
            .all(|row| row["execution"].is_null() && row["type"] != "replay-selection")
    );
    fs::remove_file(&legacy).unwrap();
    // Legacy-only installations still resolve their old default without ZIP.
    fs::rename(&bundles[0], project.0.join("saved-bundle.zip")).unwrap();
    fs::copy(project.0.join("input.jsonl"), &legacy).unwrap();
    assert!(replay_from(&project, None, &[]).status.success());
}

fn rewrite_bundle(
    project: &Project,
    edit: impl FnOnce(&mut asb_cli::cassette::Manifest, &mut std::collections::BTreeMap<u64, Vec<u8>>),
) {
    use std::io::Write;
    let path = project.0.join("new-recording.jsonl");
    let mut archive =
        asb_cli::cassette::Archive::open(&path, asb_cli::cassette::Limits::default()).unwrap();
    let mut manifest = archive.manifest().clone();
    let mut payloads = manifest
        .tracks
        .iter()
        .map(|track| (track.id, archive.read_track(track.id).unwrap()))
        .collect::<std::collections::BTreeMap<_, _>>();
    // Rewriters model a changed manifest/track, not an older recording
    // format.  Preserve every sealed sidecar (local-tool transcripts and
    // deterministic manifests included), otherwise the test fixture ceases to
    // describe the archive it writes and replay quite correctly refuses it.
    let mut artifacts = Vec::new();
    for track in &manifest.tracks {
        for artifact in &track.artifacts {
            artifacts.push((
                track.id,
                artifact.kind.clone(),
                archive.read_artifact(track.id, &artifact.kind).unwrap(),
            ));
        }
    }
    edit(&mut manifest, &mut payloads);
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("manifest.json", options).unwrap();
    zip.write_all(&serde_json::to_vec(&manifest).unwrap())
        .unwrap();
    for (id, payload) in payloads {
        zip.start_file(format!("tracks/{id}.jsonl"), options)
            .unwrap();
        zip.write_all(&payload).unwrap();
    }
    for (id, kind, bytes) in artifacts {
        zip.start_file(format!("artifacts/{id}/{kind}.jsonl"), options)
            .unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn bundled_replay_rejects_each_compatibility_difference_before_client_launch() {
    let project = Project::new();
    project.config(1);
    let provider = RecordingProvider::new();
    let recorded = record(&project, &provider, None);
    assert!(
        recorded.status.success(),
        "{}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    drop(provider);
    let path = project.0.join("new-recording.jsonl");
    let original = fs::read(&path).unwrap();
    let attempts = fs::read(project.0.join("attempts.jsonl")).unwrap();
    for field in [
        "agent", "provider", "model", "workload", "task", "dataset", "prompt", "material",
        "protocol",
    ] {
        fs::write(&path, &original).unwrap();
        rewrite_bundle(&project, |manifest, _| {
            let identity = &mut manifest.compatibility;
            match field {
                "agent" => identity.agent = "different".into(),
                "provider" => identity.provider = "different".into(),
                "model" => identity.model = "different".into(),
                "workload" => identity.workload = "different".into(),
                "task" => identity.task = "different".into(),
                "dataset" => identity.dataset_sha256 = Some("e".repeat(64)),
                "prompt" => identity.prompt_sha256 = "e".repeat(64),
                "material" => identity.material_sha256 = "e".repeat(64),
                "protocol" => identity.protocol_sha256 = Some("e".repeat(64)),
                _ => unreachable!(),
            }
        });
        let output = replay_bundle(&project, &["--repeat", "2", "--track", "1"]);
        assert!(!output.status.success(), "{field}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("differs"),
            "{field}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(project.0.join("attempts.jsonl")).unwrap(),
            attempts,
            "{field} launched a client"
        );
    }
    assert_eq!(
        fs::read_to_string(project.0.join("workspace/preserve-user-file")).unwrap(),
        "untouched user material"
    );
}

#[test]
fn bundled_replay_only_decodes_selected_entries_and_fails_closed_on_selected_corruption() {
    let project = Project::new();
    project.config(2);
    let provider = RecordingProvider::new();
    assert!(record(&project, &provider, Some("1")).status.success());
    drop(provider);
    // Keep track2's length/CRC valid but make its payload disagree with SHA.
    rewrite_bundle(&project, |_, payloads| {
        let payload = payloads.get_mut(&2).unwrap();
        let index = payload.iter().position(|byte| *byte == b'f').unwrap();
        payload[index] = b'g';
    });
    let good = replay_bundle(&project, &["--track", "1"]);
    assert!(
        good.status.success(),
        "{}",
        String::from_utf8_lossy(&good.stderr)
    );
    let attempts = fs::read(project.0.join("attempts.jsonl")).unwrap();
    let corrupt = replay_bundle(&project, &["--track", "2"]);
    assert!(!corrupt.status.success());
    assert!(
        String::from_utf8_lossy(&corrupt.stderr).contains("checksum"),
        "{}",
        String::from_utf8_lossy(&corrupt.stderr)
    );
    assert_eq!(
        fs::read(project.0.join("attempts.jsonl")).unwrap(),
        attempts
    );
}

#[test]
fn complete_failed_recording_is_automatically_replayed_without_live_credentials() {
    let project = Project::new();
    project.config(1);
    let provider = RecordingProvider::new();
    fs::write(
        project.0.join("fail-first"),
        "record a complete failed attempt",
    )
    .unwrap();
    let recorded = record(&project, &provider, None);
    assert!(!recorded.status.success());
    let archive = asb_cli::cassette::Archive::open(
        &project.0.join("new-recording.jsonl"),
        asb_cli::cassette::Limits::default(),
    )
    .unwrap();
    let track = &archive.manifest().tracks[0];
    assert_eq!(track.process_success, Some(false));
    assert!(track.automatically_selectable());
    drop(provider);
    fs::remove_file(project.0.join("fail-first")).unwrap();
    let replayed = replay_bundle(&project, &[]);
    assert!(
        replayed.status.success(),
        "{}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    let selection = records(&replayed.stdout)
        .into_iter()
        .find(|row| row["type"] == "replay-selection")
        .unwrap();
    assert_eq!(selection["selection"]["tracks"], json!([1]));
    assert!(selection["selection"]["seed"].is_u64());
}

#[test]
fn bundled_replay_keeps_private_inputs_after_source_replacement_and_allows_new_concurrency() {
    let project = Project::new();
    project.config(2);
    let provider = RecordingProvider::new();
    assert!(record(&project, &provider, Some("1")).status.success());
    drop(provider);
    let config = project.0.join("benchmark.toml");
    fs::write(
        &config,
        fs::read_to_string(&config)
            .unwrap()
            .replace("concurrency=2", "concurrency=1"),
    )
    .unwrap();
    fs::write(project.0.join("expected-concurrency"), "1").unwrap();
    fs::write(
        project.0.join("replace-bundle"),
        "replace only after staging",
    )
    .unwrap();
    let replayed = replay_bundle(&project, &["--repeat", "3", "--seed", "11"]);
    assert!(
        replayed.status.success(),
        "{}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    let summaries: Vec<_> = records(&replayed.stdout)
        .into_iter()
        .filter(|row| row.get("task").is_some())
        .collect();
    assert_eq!(summaries.len(), 3);
    assert!(summaries.iter().all(|row| row["workload_success"] == true));
    assert_eq!(
        fs::read_to_string(project.0.join("new-recording.jsonl")).unwrap(),
        "replaced after private staging"
    );
}

#[test]
fn incomplete_numeric_track_keeps_strict_full_consumption_and_continues_completed_failures() {
    let project = Project::new();
    project.config(1);
    let provider = RecordingProvider::new();
    assert!(record(&project, &provider, None).status.success());
    drop(provider);
    rewrite_bundle(&project, |manifest, payloads| {
        let payload = payloads.get_mut(&1).unwrap();
        payload.extend_from_within(..);
        let track = &mut manifest.tracks[0];
        track.complete = false;
        track.bytes = payload.len() as u64;
        track.sha256 = asb_cli::cassette::digest(payload);
        track.statistics.requests = Some(2);
    });
    let attempts = records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len();
    let automatic = replay_bundle(&project, &[]);
    assert!(!automatic.status.success());
    assert!(String::from_utf8_lossy(&automatic.stderr).contains("no complete nonempty"));
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        attempts
    );
    let explicit = replay_bundle(&project, &["--repeat", "2", "--track", "1"]);
    assert!(!explicit.status.success());
    let disclosure = records(&explicit.stdout);
    let warning_index = disclosure
        .iter()
        .position(|row| row["type"] == "replay-warning")
        .unwrap();
    let warning = &disclosure[warning_index];
    assert_eq!(warning["code"], "incomplete-track");
    assert_eq!(warning["track_ids"], json!([1]));
    assert_eq!(warning["complete"], false);
    assert_eq!(warning["strict_matching"], true);
    assert_eq!(warning["full_consumption_required"], true);
    assert!(
        warning_index
            < disclosure
                .iter()
                .position(|row| row.get("task").is_some())
                .unwrap()
    );
    let plan_path = disclosure
        .iter()
        .find(|row| row["type"] == "execution-plan")
        .unwrap()["path"]
        .as_str()
        .unwrap();
    let plan: Value = serde_json::from_slice(&fs::read(plan_path).unwrap()).unwrap();
    assert_eq!(plan["replay_inputs"][0]["complete"], false);
    assert!(
        String::from_utf8_lossy(&explicit.stderr).contains("consumed 1 of 2"),
        "{}",
        String::from_utf8_lossy(&explicit.stderr)
    );
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        attempts + 2
    );
    let rows: Vec<_> = records(&explicit.stdout)
        .into_iter()
        .filter(|row| row.get("task").is_some())
        .collect();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row["process_success"] == true && row["workload_success"] == true)
    );
    let human = replay_with_format(
        &project,
        Some("new-recording.jsonl"),
        &["--track", "1"],
        false,
    );
    assert!(!human.status.success());
    let text = String::from_utf8(human.stdout).unwrap();
    let warning = text
        .find("Warning: selected track(s) 1 are incomplete")
        .unwrap();
    assert!(warning < text.find("success_rate=").unwrap());
    assert!(text.contains("strict matching and full consumption are still required"));
}

#[test]
fn replay_selection_flags_reject_invalid_modes_and_keep_legacy_metadata_unknown() {
    let project = Project::new();
    project.config(1);
    for flags in [
        vec!["--track", "1"],
        vec!["--seed", "2"],
        vec!["--record", "--track", "1"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_asb"))
            .current_dir(&project.0)
            .isolated_env()
            .args(["run", "benchmark.toml"])
            .args(flags)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!project.0.join("results").exists());
    }
    for flags in [vec!["--seed", "2"], vec!["--track", "min:tokens"]] {
        let output = project.run_extra("1", &flags);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("legacy JSONL"));
        assert!(!project.0.join("results").exists());
    }
    let legacy = project.0.join("input.jsonl");
    fs::rename(&legacy, project.0.join("input-concurrency-64.jsonl")).unwrap();
    let output = project.run_extra("1", &["--track", "64"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = records(&output.stdout);
    let summary = rows.iter().find(|row| row.get("task").is_some()).unwrap();
    assert_eq!(summary["execution"]["replay_track"], 64);
    assert!(summary["execution"].get("replay_seed").is_none());
    assert!(summary["execution"].get("replay_algorithm").is_none());
    assert!(rows.iter().all(|row| row["type"] != "replay-selection"));
}

#[test]
fn bundled_replay_choices_drive_actual_private_batch_inputs_without_a_provider() {
    let project = Project::new();
    project.config(2);
    let provider = RecordingProvider::new();
    let recorded = record(&project, &provider, Some("2"));
    assert!(
        recorded.status.success(),
        "{}",
        String::from_utf8_lossy(&recorded.stderr)
    );
    let mut archive = asb_cli::cassette::Archive::open(
        &project.0.join("new-recording.jsonl"),
        asb_cli::cassette::Limits::default(),
    )
    .unwrap();
    let mut tags = std::collections::BTreeMap::new();
    for id in 1..=4 {
        let payload = archive.read_track(id).unwrap();
        let row: Value = serde_json::from_slice(
            payload
                .split(|byte| *byte == b'\n')
                .find(|row| !row.is_empty())
                .unwrap(),
        )
        .unwrap();
        let response: Value =
            serde_json::from_str(row["response"]["body"].as_str().unwrap()).unwrap();
        tags.insert(id, response["recorded_track"].clone());
    }
    let before = provider.calls.load(std::sync::atomic::Ordering::SeqCst);
    drop(provider);
    let mut observed_selections = Vec::new();
    for extra in [
        vec!["--repeat", "3", "--seed", "19"],
        vec!["--repeat", "3", "--seed", "19"],
        vec!["--repeat", "2", "--track", "3"],
        vec!["--track", "max:tokens"],
    ] {
        if !observed_selections.is_empty() {
            fs::write(
                project.0.join("reverse-arrivals"),
                "hold replica1 after selection",
            )
            .unwrap();
        }
        let attempt_offset = records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len();
        let replayed = replay_bundle(&project, &extra);
        assert!(
            replayed.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&replayed.stdout),
            String::from_utf8_lossy(&replayed.stderr)
        );
        let rows = records(&replayed.stdout);
        let selection = rows
            .iter()
            .find(|row| row["type"] == "replay-selection")
            .unwrap()["selection"]
            .clone();
        let plan_path = rows
            .iter()
            .find(|row| row["type"] == "execution-plan")
            .unwrap()["path"]
            .as_str()
            .unwrap();
        let plan: Value = serde_json::from_slice(&fs::read(plan_path).unwrap()).unwrap();
        assert_eq!(plan["replay_selection"], selection);
        let chosen = selection["tracks"].as_array().unwrap();
        let attempts = records(&fs::read(project.0.join("attempts.jsonl")).unwrap());
        assert_eq!(attempts.len() - attempt_offset, chosen.len());
        for row in &attempts[attempt_offset..] {
            let slot =
                (row["batch"].as_u64().unwrap() - 1) * 2 + row["replica"].as_u64().unwrap() - 1;
            let id = chosen[slot as usize].as_u64().unwrap();
            assert_eq!(row["response_tag"], tags[&id], "wrong batch/replica input");
            assert!(!row["before"].as_str().unwrap().contains("if segment"));
        }
        for row in rows.iter().filter(|row| row.get("task").is_some()) {
            let slot = row["execution"]["slot"].as_u64().unwrap() as usize - 1;
            assert_eq!(row["execution"]["replay_track"], chosen[slot]);
            assert_eq!(row["execution"]["replay_seed"], selection["seed"]);
            assert_eq!(row["execution"]["replay_algorithm"], selection["algorithm"]);
        }
        if extra.contains(&"3") && extra.contains(&"--track") {
            assert!(chosen.iter().all(|id| *id == 3));
        }
        if extra.contains(&"max:tokens") {
            assert_eq!(chosen, &vec![json!(1), json!(1)]);
        }
        observed_selections.push(selection);
    }
    assert_eq!(observed_selections[0], observed_selections[1]);
    assert_eq!(before, 4);
}

#[test]
fn recording_population_metadata_budget_fails_before_any_client_or_inference() {
    let project = Project::new();
    project.config(1);
    let provider = RecordingProvider::new();
    let output = record(&project, &provider, Some("60000"));
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("metadata"), "{diagnostic}");
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(!project.0.join("attempts.jsonl").exists());
    assert!(!project.0.join("barrier-1-1").exists());
    for name in [
        "new-recording.jsonl",
        ".new-recording.jsonl.partial",
        ".new-recording.jsonl.recording.lock",
    ] {
        assert!(
            !project.0.join(name).exists(),
            "unexpected recording output {name}"
        );
    }
    assert_eq!(
        fs::read_to_string(project.0.join("workspace/preserve-user-file")).unwrap(),
        "untouched user material"
    );
}

#[test]
fn raw_custody_consumption_keeps_true_false_track_grades_and_actual_offline_replay() {
    for grade in [true, false] {
        let project = Project::new();
        project.config(1);
        if !grade {
            fs::write(
                project.0.join("false-grade-first"),
                "leave original broken file",
            )
            .unwrap();
        }
        let provider = RecordingProvider::new();
        let recorded = record(&project, &provider, None);
        assert_eq!(
            recorded.status.success(),
            grade,
            "{}\n{}",
            String::from_utf8_lossy(&recorded.stdout),
            String::from_utf8_lossy(&recorded.stderr)
        );
        let rows = records(&recorded.stdout);
        let summary = rows.iter().find(|row| row.get("task").is_some()).unwrap();
        assert_eq!(summary["process_success"], true);
        assert_eq!(summary["workload_success"], grade);
        let raw = PathBuf::from(summary["recording"]["producer"].as_str().unwrap());
        assert!(!raw.exists());
        let parent = raw.parent().unwrap();
        let custody: Vec<_> = fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".asb-raw-custody-")
            })
            .collect();
        assert_eq!(custody.len(), 1);
        assert!(fs::read_dir(&custody[0]).unwrap().next().is_none());
        assert_eq!(
            fs::read_dir(parent.join(".asb-completions"))
                .unwrap()
                .count(),
            1,
            "confirmed ordinary grades retain their existing continuation record"
        );
        let evidence: asb_cli::cassette::TrackEvidence =
            serde_json::from_value(summary["recording"]["track"].clone()).unwrap();
        evidence
            .verify(asb_cli::cassette::Limits::default())
            .unwrap();
        let mut archive = asb_cli::cassette::Archive::open(
            &project.0.join("new-recording.jsonl"),
            asb_cli::cassette::Limits::default(),
        )
        .unwrap();
        assert_eq!(archive.manifest().tracks[0].process_success, Some(true));
        assert_eq!(archive.manifest().tracks[0].workload_success, Some(grade));
        assert!(!archive.read_track(1).unwrap().is_empty());
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(provider);
        // Actual selected-track replay after closing the only loopback fixture
        // provider, not a reconstructed result or a second recording attempt.
        let replayed = replay_bundle(&project, &["--track", "1"]);
        assert_eq!(
            replayed.status.success(),
            grade,
            "{}\n{}",
            String::from_utf8_lossy(&replayed.stdout),
            String::from_utf8_lossy(&replayed.stderr)
        );
        let replay_rows = records(&replayed.stdout);
        let replay_summary = replay_rows
            .iter()
            .find(|row| row.get("task").is_some())
            .unwrap();
        assert_eq!(replay_summary["workload_success"], grade);
        assert_eq!(replay_summary["execution"]["replay_track"], 1);
        assert_eq!(
            records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
            2
        );
        assert_eq!(
            fs::read_to_string(project.0.join("workspace/preserve-user-file")).unwrap(),
            "untouched user material"
        );
    }
}

#[test]
fn bundled_recording_uses_fresh_default_and_stable_repeated_slots_even_with_jsonl_suffix() {
    for (repeat, concurrency) in [(None, 1), (Some("2"), 2)] {
        let project = Project::new();
        project.config(concurrency);
        let provider = RecordingProvider::new();
        if repeat.is_some() {
            fs::write(project.0.join("fail-first"), "ordinary failed agent").unwrap();
        }
        let output = record(&project, &provider, repeat);
        assert_eq!(
            output.status.success(),
            repeat.is_none(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let rows = records(&output.stdout);
        let summaries: Vec<_> = rows
            .iter()
            .filter(|row| row.get("task").is_some())
            .collect();
        let expected = if repeat.is_some() { 4 } else { 1 };
        assert_eq!(summaries.len(), expected, "{rows:?}");
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            expected
        );
        let destination = project.0.join("new-recording.jsonl");
        assert_eq!(&fs::read(&destination).unwrap()[..2], b"PK");
        let archive =
            asb_cli::cassette::Archive::open(&destination, asb_cli::cassette::Limits::default())
                .unwrap();
        assert_eq!(archive.manifest().tracks.len(), expected);
        assert!(archive.manifest().compatibility.protocol_sha256.is_some());
        for (index, track) in archive.manifest().tracks.iter().enumerate() {
            assert_eq!(track.id, index as u64 + 1);
            assert!(track.complete);
            assert!(track.automatically_selectable());
            assert_eq!(track.statistics.requests, Some(1));
            assert_eq!(track.statistics.tokens, Some(5));
            let summary = summaries[index];
            let evidence: asb_cli::cassette::TrackEvidence =
                serde_json::from_value(summary["recording"]["track"].clone()).unwrap();
            evidence
                .verify(asb_cli::cassette::Limits::default())
                .unwrap();
            assert!(!PathBuf::from(summary["recording"]["producer"].as_str().unwrap()).exists());
        }
        if repeat.is_some() {
            assert_eq!(archive.manifest().tracks[0].process_success, Some(false));
        }
        assert_eq!(
            fs::read_to_string(project.0.join("workspace/preserve-user-file")).unwrap(),
            "untouched user material"
        );
        let before = provider.calls.load(std::sync::atomic::Ordering::SeqCst);
        let refused = record(&project, &provider, repeat);
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("already exists"));
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            before
        );
    }
}

#[test]
fn complete_llm_recording_keeps_a_drained_truncated_transcript_replayable() {
    let project = Project::new();
    project.config(2);
    fs::write(
        project.0.join("noisy-record"),
        "overflow only the first client transcript",
    )
    .unwrap();
    let provider = RecordingProvider::new();
    let output = record(&project, &provider, Some("2"));
    assert!(output.status.success());
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 4);
    let rows = records(&output.stdout);
    let summaries: Vec<_> = rows
        .iter()
        .filter(|row| row.get("task").is_some())
        .collect();
    assert_eq!(summaries.len(), 4);
    assert!(
        summaries
            .iter()
            .all(|summary| summary["workload_success"] == true)
    );
    let incomplete = summaries
        .iter()
        .find(|summary| summary["replica_index"] == 1)
        .unwrap();
    assert_eq!(incomplete["process_success"], true);
    assert_eq!(
        summaries
            .iter()
            .find(|summary| summary["replica_index"] == 2)
            .unwrap()["process_success"],
        true
    );
    assert!(incomplete["failure_reason"].is_null());
    assert_eq!(incomplete["token_usage"]["total_tokens"], 5);
    assert!(incomplete["steps"].is_null());
    let root = PathBuf::from(
        rows.iter()
            .find(|row| row["type"] == "execution-plan")
            .unwrap()["path"]
            .as_str()
            .unwrap(),
    )
    .parent()
    .unwrap()
    .to_path_buf();
    assert!(root.join("batch-2").exists());
    assert!(root.join("batch-1/outcomes/1.json").is_file());
    let log = records(&fs::read(root.join("batch-1/run-concurrency-1.jsonl")).unwrap());
    assert_eq!(log.last().unwrap()["exit_code"], 0);
    let archive = asb_cli::cassette::Archive::open(
        &project.0.join("new-recording.jsonl"),
        asb_cli::cassette::Limits::default(),
    )
    .unwrap();
    assert_eq!(archive.manifest().tracks.len(), 4);
    assert_eq!(archive.manifest().tracks[0].process_success, Some(true));
    for track in &archive.manifest().tracks {
        assert!(track.complete && track.automatically_selectable());
        assert_eq!(track.statistics.tokens, Some(5));
        assert_eq!(track.statistics.requests, Some(1));
    }
    drop(archive);
    let plan = root.join("plan.json");
    let compared = compare_campaign(&project, &plan);
    assert!(
        compared.status.success(),
        "{}",
        String::from_utf8_lossy(&compared.stderr)
    );
    let report = root.join("comparison/comparison.json");
    let previous = fs::read(&report).unwrap();
    // The overflow slot has only a reporting log, not an outcome receipt.
    // Its recording association must still verify the published payload.
    fs::remove_file(project.0.join("new-recording.jsonl")).unwrap();
    let corrupt = compare_campaign(&project, &plan);
    assert!(!corrupt.status.success());
    assert_eq!(fs::read(report).unwrap(), previous);
}

#[test]
fn bundle_commit_failure_keeps_real_grades_and_stops_before_another_batch() {
    let project = Project::new();
    project.config(2);
    let config = project.0.join("benchmark.toml");
    fs::write(
        &config,
        format!(
            "{}\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=[]\n",
            fs::read_to_string(&config).unwrap()
        ),
    )
    .unwrap();
    fs::write(
        project.0.join("block-bundle-commit"),
        "force create-new staging failure",
    )
    .unwrap();
    let provider = RecordingProvider::new();
    let output = record(&project, &provider, Some("2"));
    assert!(!output.status.success());
    let rows = records(&output.stdout);
    let summaries: Vec<_> = rows
        .iter()
        .filter(|row| row.get("task").is_some())
        .collect();
    assert_eq!(summaries.len(), 2, "{rows:?}");
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(
        summaries
            .iter()
            .any(|summary| summary["failure_kind"] == "recording_error")
    );
    for summary in &summaries {
        assert_eq!(summary["process_success"], true);
        assert_eq!(summary["workload_success"], true);
        assert_eq!(summary["execution"]["batch"], 1);
    }
    let root = PathBuf::from(rows[0]["path"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    for summary in summaries
        .iter()
        .filter(|summary| summary["failure_kind"] == "recording_error")
    {
        let replica = summary["replica_index"].as_u64().unwrap();
        let log = records(
            &fs::read(root.join(format!("batch-1/run-concurrency-{replica}.jsonl"))).unwrap(),
        );
        let measured: Value = serde_json::from_slice(
            &fs::read(root.join(format!(
                "batch-1/measurements-concurrency-{replica}/run.json"
            )))
            .unwrap(),
        )
        .unwrap();
        for record in [log.last().unwrap(), &measured] {
            assert_eq!(record["status"], "failed");
            assert_eq!(record["failure_kind"], "recording_error");
            assert_eq!(record["process_success"], true);
            assert_eq!(record["workload_success"], true);
        }
        // Incoming main deliberately retains grade-derived legacy success.
        assert_eq!(measured["success"], true);
    }
    assert!(!root.join("batch-2").exists());
    assert!(!project.0.join("new-recording.jsonl").exists());
    assert_eq!(
        fs::read_to_string(project.0.join(".new-recording.jsonl.partial/track-1.jsonl")).unwrap(),
        "preserved blocking stage"
    );
    let finished: Value =
        serde_json::from_slice(&fs::read(root.join("finished.json")).unwrap()).unwrap();
    assert_eq!(finished["cassette_publication"]["status"], "failed");
    assert!(root.join("batch-1/capture-concurrency-1.jsonl").is_file());
}

#[test]
fn missing_optional_outcome_keeps_grade_but_stops_repeated_batch_admission() {
    let project = Project::new();
    project.config(1);
    fs::write(
        project.0.join("block-outcome-first"),
        "inject outcome storage failure",
    )
    .unwrap();
    let output = project.run("2");
    assert!(!output.status.success());
    let rows = records(&output.stdout);
    let summaries: Vec<_> = rows
        .iter()
        .filter(|row| row.get("process_success").is_some())
        .collect();
    assert_eq!(summaries.len(), 1, "{rows:?}");
    assert_eq!(summaries[0]["process_success"], true);
    assert_eq!(summaries[0]["workload_success"], true);
    assert!(
        rows.iter().any(|row| row["event"] == "continuation_warning"
            && row["code"] == "task_outcome_unavailable")
    );
    assert_eq!(
        fs::read_to_string(project.0.join("attempts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(!project.0.join("barrier-2-1").exists());
}

#[test]
fn truncated_capture_preserves_file_grades_and_allows_the_next_batch() {
    for grade in [true, false] {
        let project = Project::new();
        project.config(2);
        fs::write(
            project.0.join("capture-overflow-first"),
            "overflow one admitted slot",
        )
        .unwrap();
        if !grade {
            fs::write(
                project.0.join("false-grade-first"),
                "retain independently failed file grade",
            )
            .unwrap();
        }
        let output = project.run("2");
        assert_eq!(output.status.success(), grade);
        let rows = records(&output.stdout);
        let summaries: Vec<_> = rows
            .iter()
            .filter(|row| row.get("task").is_some())
            .collect();
        assert_eq!(summaries.len(), 4, "both continued batches must finish");
        let truncated = summaries
            .iter()
            .find(|row| row["execution"]["replica"] == 1)
            .unwrap();
        // A bounded transcript prefix does not make an otherwise drained
        // execution incomplete; its independent evaluator result remains
        // authoritative.
        assert_eq!(truncated["process_success"], true);
        assert_eq!(truncated["workload_success"], grade);
        let campaign = PathBuf::from(rows[0]["path"].as_str().unwrap())
            .parent()
            .unwrap()
            .to_path_buf();
        assert!(campaign.join("batch-2").exists());
        assert!(campaign.join("batch-1/outcomes/1.json").is_file());
        assert!(campaign.join("batch-1/outcomes/2.json").is_file());
        assert!(!campaign.join(".checkpoint").exists());
        let log: Value = serde_json::from_slice(
            &fs::read(campaign.join("batch-1/run-concurrency-1.jsonl")).unwrap(),
        )
        .unwrap();
        assert_eq!(log["workload_success"], grade);
        assert_eq!(
            log["workload_resource_disposition"],
            truncated["workload_resource_disposition"]
        );
        assert_eq!(log["workload_artifacts"], truncated["workload_artifacts"]);
        let finished: Value =
            serde_json::from_slice(&fs::read(campaign.join("finished.json")).unwrap()).unwrap();
        assert_eq!(finished["stopped"], false);
        assert_eq!(
            fs::read_to_string(project.0.join("attempts.jsonl"))
                .unwrap()
                .lines()
                .count(),
            4
        );
        assert!(project.0.join("barrier-2-1").exists());
        let private = campaign.join("batch-1/.asb-completions");
        for entry in fs::read_dir(private).unwrap() {
            let record: Value =
                serde_json::from_slice(&fs::read(entry.unwrap().path()).unwrap()).unwrap();
            assert!(record["summary"]["execution"]["replica"].is_u64());
        }
    }
}

#[test]
fn unknown_grading_outcome_remains_null_and_later_fresh_batches_continue() {
    let project = Project::new();
    project.config(2);
    fs::write(
        project.0.join("grader-error-first"),
        "inject invalid evaluator input",
    )
    .unwrap();
    let output = project.run("2");
    assert!(!output.status.success());
    let rows = records(&output.stdout);
    assert_eq!(rows.len(), 5, "{}", String::from_utf8_lossy(&output.stderr));
    let failed = &rows[1];
    assert_eq!(failed["process_success"], true);
    assert!(failed["workload_success"].is_null());
    assert!(failed["success_rate"].is_null());
    assert_eq!(failed["failure_kind"], "grader_error");
    assert_eq!(failed["workload_resource_disposition"], "not-required");
    let mut attempts = std::collections::BTreeSet::new();
    for row in &rows[1..] {
        assert!(attempts.insert(row["task"]["attempt_id"].as_str().unwrap()));
        assert_eq!(row["process_success"], true);
    }
    assert_eq!(attempts.len(), 4);
    assert!(rows[2..].iter().all(|row| row["workload_success"] == true));
    let campaign = PathBuf::from(rows[0]["path"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    let finished: Value =
        serde_json::from_slice(&fs::read(campaign.join("finished.json")).unwrap()).unwrap();
    assert_eq!(finished["completed_batches"], 2);
    assert_eq!(finished["failed_batches"], 1);
    assert_eq!(finished["stopped"], false);
}

#[test]
fn repeated_native_clients_have_fresh_home_xdg_and_temporary_state() {
    let project = Project::new();
    project.config(2);
    let state_names = [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_RUNTIME_DIR",
        "TMPDIR",
    ];
    let script = project.0.join("fixture-agent");
    let original = fs::read_to_string(&script).unwrap();
    let check = r#"
names = ['HOME','XDG_CONFIG_HOME','XDG_DATA_HOME','XDG_CACHE_HOME','XDG_STATE_HOME','XDG_RUNTIME_DIR','TMPDIR']
(root/f'state-observed-{batch}-{replica}.json').write_text(json.dumps({name:os.environ.get(name) for name in names}))
state = {}
for name in names:
    directory = pathlib.Path(os.environ[name])
    assert directory.is_absolute() and directory != root/'parent-state'/name, 'inherited mutable state: '+name
    assert directory != root and workspace not in directory.parents, 'state must be outside workload'
    assert directory.is_dir() and directory.stat().st_mode & 0o777 == 0o700, 'private state directory required'
    assert not list(directory.iterdir()), 'state was not fresh: '+name
    (directory/'owned-session').write_text(str(os.getpid()))
    state[name] = str(directory)
assert os.environ['ASB_MODEL'] == 'fixture', 'model configuration lost'
assert all(name not in os.environ for name in ['CODEX_HOME','PI_CODING_AGENT_DIR','HERMES_HOME','CODEX_SESSION_ID']), 'parent agent state leaked'
(root/f'state-{batch}-{replica}.json').write_text(json.dumps(state))
"#;
    let modified = original
        .replace(
            "before = (workspace/",
            &format!("{check}\nbefore = (workspace/"),
        )
        .replace("runtime.mkdir()", "runtime.mkdir(exist_ok=True)");
    assert_ne!(modified, original);
    fixture_writer::write_file(&project.0, &script, modified).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_asb"));
    command
        .current_dir(&project.0)
        .isolated_env()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("CODEX_HOME", project.0.join("parent-state/CODEX_HOME"))
        .env(
            "PI_CODING_AGENT_DIR",
            project.0.join("parent-state/PI_CODING_AGENT_DIR"),
        )
        .env("HERMES_HOME", project.0.join("parent-state/HERMES_HOME"))
        .env("CODEX_SESSION_ID", "parent-session-fixture")
        .args([
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "input.jsonl",
            "--repeat",
            "2",
            "--json",
        ]);
    for name in state_names {
        let directory = project.0.join("parent-state").join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("parent-marker"), "preserve inherited state").unwrap();
        command.env(name, directory);
    }
    let output = command.output().unwrap();
    let observed = fs::read_to_string(project.0.join("state-observed-1-1.json")).unwrap();
    assert!(
        output.status.success(),
        "observed state={observed}\nstdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut roots = std::collections::BTreeSet::new();
    for batch in 1..=2 {
        for replica in 1..=2 {
            let state: Value = serde_json::from_slice(
                &fs::read(project.0.join(format!("state-{batch}-{replica}.json"))).unwrap(),
            )
            .unwrap();
            for name in state_names {
                assert!(
                    roots.insert(state[name].as_str().unwrap().to_owned()),
                    "shared state path"
                );
            }
        }
    }
    assert_eq!(roots.len(), 28);
    for name in state_names {
        let directory = project.0.join("parent-state").join(name);
        assert_eq!(
            fs::read_to_string(directory.join("parent-marker")).unwrap(),
            "preserve inherited state"
        );
        assert_eq!(fs::read_dir(directory).unwrap().count(), 1);
    }
}

#[test]
fn measurement_overrides_never_choose_perfetto_authority_from_output_location() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    let foreign = Project::new();
    let outside = Project::new();
    // A writable non-project destination, and a destination under a differently
    // configured project, must both retain the benchmark's own authority.
    fs::remove_file(outside.0.join(".asb/project.toml")).unwrap();
    fs::write(
        foreign.0.join(".asb/project.toml"),
        "perfetto_instances=['foreign-instance']\n",
    )
    .unwrap();
    project.config(2);
    let path = project.0.join("benchmark.toml");
    let config = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("{config}\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['perfetto']\n")).unwrap();
    let tools = project.0.join("tools/perfetto/bin");
    fs::create_dir_all(&tools).unwrap();
    for name in ["tracebox", "trace_processor_shell"] {
        let path = tools.join(name);
        fixture_writer::write_file(&project.0, &path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    for destination in [
        outside.0.join("measurements"),
        foreign.0.join("measurements"),
    ] {
        let output = project.run_extra("2", &["--measurements", destination.to_str().unwrap()]);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("Perfetto needs delegated, isolated tracefs instances"),
            "{error}"
        );
        assert!(!error.contains("configuration does not belong"), "{error}");
        assert!(!project.0.join("attempts.jsonl").exists());
        assert!(!foreign.0.join(".asb/perfetto").exists());
        assert!(!outside.0.join(".asb/perfetto").exists());
    }
}

#[test]
fn legacy_native_run_keeps_explicit_parent_home_and_xdg_state() {
    let project = Project::new();
    project.config(1);
    fixture_writer::write_file(&project.0, &project.0.join("fixture-agent"),r#"#!/usr/bin/env python3
import json,os,pathlib,urllib.request
root=pathlib.Path(__file__).parent
(root/'legacy-state.json').write_text(json.dumps({name:os.environ.get(name) for name in ['HOME','XDG_STATE_HOME']}))
body={'model':'fixture','tools':[{'type':'function','function':{'name':'fixture','parameters':{'type':'object'}}}],'messages':[]}
url=os.environ['ASB_PROVIDER_BASE_URL'].rstrip('/')
if url.endswith('/v1'): url=url[:-3]
with urllib.request.urlopen(urllib.request.Request(url+'/v1/chat/completions',data=json.dumps(body).encode(),headers={'Content-Type':'application/json'}),timeout=5) as response:
    assert response.status==200
pathlib.Path('src/parser.py').write_text("def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n")
"#).unwrap();
    let state = project.0.join("parent-xdg");
    fs::create_dir(&state).unwrap();
    fs::write(state.join("preserve"), "parent state").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_asb"))
        .current_dir(&project.0)
        .isolated_env()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &project.0)
        .env("XDG_STATE_HOME", &state)
        .args([
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "input.jsonl",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let observed: Value =
        serde_json::from_slice(&fs::read(project.0.join("legacy-state.json")).unwrap()).unwrap();
    assert_eq!(observed["HOME"], project.0.to_str().unwrap());
    assert_eq!(observed["XDG_STATE_HOME"], state.to_str().unwrap());
    assert_eq!(
        fs::read_to_string(state.join("preserve")).unwrap(),
        "parent state"
    );
}

#[test]
fn repeats_fresh_batches_preserve_users_collect_all_outcomes_and_continue_ordinary_failures() {
    let project = Project::new();
    project.config(2);
    fs::write(project.0.join("fail-first"), "fixture").unwrap();
    let output = project.run_extra("3", &["--measurements", "measurements"]);
    assert!(!output.status.success());
    let out = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("exit status: 7") && err.contains("failed"),
        "stdout={out} stderr={err}"
    );
    let attempts = records(&fs::read(project.0.join("attempts.jsonl")).unwrap());
    assert_eq!(attempts.len(), 6);
    let initial = attempts[0]["before"].as_str().unwrap();
    assert!(initial.contains("split"));
    assert!(attempts.iter().all(|row| row["before"] == initial));
    let paths: std::collections::BTreeSet<_> = attempts
        .iter()
        .map(|row| row["workspace"].as_str().unwrap())
        .collect();
    assert_eq!(paths.len(), 6);
    for batch in 1..3 {
        let previous = attempts
            .iter()
            .filter(|row| row["batch"] == batch)
            .map(|row| row["end"].as_u64().unwrap())
            .max()
            .unwrap();
        let next = attempts
            .iter()
            .filter(|row| row["batch"] == batch + 1)
            .map(|row| row["start"].as_u64().unwrap())
            .min()
            .unwrap();
        assert!(previous < next);
    }
    for batch in 1..=3 {
        let latest_start = attempts
            .iter()
            .filter(|row| row["batch"] == batch)
            .map(|row| row["start"].as_u64().unwrap())
            .max()
            .unwrap();
        let earliest_end = attempts
            .iter()
            .filter(|row| row["batch"] == batch)
            .map(|row| row["end"].as_u64().unwrap())
            .min()
            .unwrap();
        assert!(
            latest_start < earliest_end,
            "batch {batch} replicas must overlap"
        );
    }
    let rows = records(&output.stdout);
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[0]["type"], "execution-plan");
    let root = PathBuf::from(rows[0]["path"].as_str().unwrap());
    let plan: Value = serde_json::from_slice(&fs::read(&root).unwrap()).unwrap();
    assert_eq!(plan["slots"].as_array().unwrap().len(), 6);
    assert_eq!(plan["replay_inputs"].as_array().unwrap().len(), 1);
    let root = root.parent().unwrap();
    for (index, row) in rows[1..].iter().enumerate() {
        assert_eq!(row["execution"]["slot"], index + 1);
        assert_eq!(row["workload_success"], true);
        assert_eq!(row["execution"]["total"], 6);
        assert_eq!(row["planning"]["repetition"]["index"], index / 2 + 1);
        assert_eq!(row["planning"]["repetition"]["total"], 3);
        assert_eq!(row["execution"]["admission"], plan["admission"]);
        assert_eq!(
            row["execution_plan_fingerprint"],
            rows[1]["execution_plan_fingerprint"]
        );
        let policy: Value =
            serde_json::from_slice(&fs::read(row["execution_plan"].as_str().unwrap()).unwrap())
                .unwrap();
        assert_eq!(policy["policy"], row["planning"]);
        let dispatch = json!({"schema_version":1,"configured":2,"admitted":2});
        assert_eq!(row["execution_cohort"]["dispatch"], dispatch);
        assert_eq!(policy["dispatch"], dispatch);
        let measured: Value = serde_json::from_slice(
            &fs::read(
                PathBuf::from(row["measurement_directory"].as_str().unwrap()).join("run.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(measured["execution_cohort"], row["execution_cohort"]);
        let batch = index / 2 + 1;
        let replica = index % 2 + 1;
        let start: Value = serde_json::from_slice(
            &fs::read(root.join(format!("batch-{batch}/starts/{replica}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(start["execution_cohort"], row["execution_cohort"]);
        let log: Value = serde_json::from_slice(
            &fs::read(root.join(format!("batch-{batch}/run-concurrency-{replica}.jsonl"))).unwrap(),
        )
        .unwrap();
        assert_eq!(log["execution_cohort"], row["execution_cohort"]);
    }
    assert_eq!(plan["admission"]["material_seconds"], 1800);
    assert_eq!(plan["admission"]["launch_seconds"], 1800);
    for batch in 1..=3 {
        assert!(root.join(format!("batch-{batch}/prepared.json")).is_file());
        assert_eq!(
            fs::read_dir(root.join(format!("batch-{batch}/outcomes")))
                .unwrap()
                .count(),
            2
        );
    }
    assert_eq!(
        fs::read_to_string(project.0.join("workspace/preserve-user-file")).unwrap(),
        "untouched user material"
    );
    assert_eq!(
        fs::read_dir(project.0.join("workspace")).unwrap().count(),
        1
    );
    assert!(!project.0.join("original.log").exists());
}

#[test]
fn repeated_execution_rejects_conflicting_manual_identity_and_invalid_policy_before_publication() {
    for extra in [
        "[planning.repetition]\nindex=2\ntotal=3\n",
        "[planning]\nrequired_policies=['not-enforceable']\n",
    ] {
        let project = Project::new();
        project.config(2);
        let config = project.0.join("benchmark.toml");
        fs::write(
            &config,
            format!("{}{extra}", fs::read_to_string(&config).unwrap()),
        )
        .unwrap();
        let output = project.run("2");
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!project.0.join("results").exists());
        assert!(!project.0.join("attempts.jsonl").exists());
        assert!(!project.0.join(".asb/execution-plans").exists());
    }
}

#[test]
fn repeated_native_clients_obey_selected_execution_cap_without_stopping_later_batches() {
    let project = Project::new();
    project.config(2);
    let config = project.0.join("benchmark.toml");
    fs::write(
        &config,
        format!(
            "{}[planning.budgets]\nexecution_seconds=1\n",
            fs::read_to_string(&config).unwrap()
        ),
    )
    .unwrap();
    let client = project.0.join("fixture-agent");
    fixture_writer::write_file(
        &project.0,
        &client,
        fs::read_to_string(&client)
            .unwrap()
            .replace("time.sleep(.06)", "time.sleep(5)"),
    )
    .unwrap();
    let output = project.run("2");
    assert!(!output.status.success());
    let rows = records(&output.stdout);
    assert_eq!(rows.len(), 5, "{}", String::from_utf8_lossy(&output.stderr));
    for row in &rows[1..] {
        assert_eq!(row["planning"]["budgets"]["execution_seconds"], 1);
        assert_eq!(row["effective_budgets"]["execution_seconds"], 1.0);
        assert_eq!(row["process_success"], false);
        assert_eq!(row["workload_success"], false);
        assert!(row["elapsed_ms"].as_u64().unwrap() < 5000);
    }
    let campaign = PathBuf::from(rows[0]["path"].as_str().unwrap());
    let finished: Value = serde_json::from_slice(
        &fs::read(campaign.parent().unwrap().join("finished.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(finished["completed_batches"], 2);
    assert_eq!(finished["stopped"], false);
}

#[test]
fn replay_inputs_are_bound_and_private_even_when_original_changes_between_batches() {
    let project = Project::new();
    project.config(2);
    let original = fs::read(project.0.join("input.jsonl")).unwrap();
    fs::write(project.0.join("mutate-source"), "fixture").unwrap();
    let output = project.run("2");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = records(&output.stdout);
    let path = PathBuf::from(rows[0]["path"].as_str().unwrap());
    let plan: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        plan["replay_inputs"][0]["sha256"],
        asb_cli::cassette::digest(&original)
    );
    assert_eq!(plan["replay_inputs"][0]["bytes"], original.len());
    let private = path
        .parent()
        .unwrap()
        .join(plan["replay_inputs"][0]["private"].as_str().unwrap());
    assert_eq!(fs::read(private).unwrap(), original);
    assert_ne!(fs::read(project.0.join("input.jsonl")).unwrap(), original);
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        4
    );
}

#[test]
fn changed_prompt_stops_later_agents_and_preparation_errors_retain_actionable_reason() {
    for first_batch_failure in [false, true] {
        let project = Project::new();
        project.config(2);
        if first_batch_failure {
            fs::write(project.0.join("workloads/bug-fix.txt"), [255]).unwrap();
        } else {
            fs::write(project.0.join("mutate-prompt"), "fixture").unwrap();
        }
        let output = project.run("3");
        assert!(!output.status.success());
        let expected = if first_batch_failure {
            "UTF-8"
        } else {
            "differs within/across batches"
        };
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rows = records(&output.stdout);
        let plan = PathBuf::from(rows[0]["path"].as_str().unwrap());
        let root = plan.parent().unwrap();
        let failed_batch = if first_batch_failure { 1 } else { 2 };
        let finished: Value = serde_json::from_slice(
            &fs::read(root.join(format!("batch-{failed_batch}/finished.json"))).unwrap(),
        )
        .unwrap();
        assert!(
            finished["failure_reason"]
                .as_str()
                .unwrap()
                .contains(expected)
        );
        assert_eq!(finished["stopped"], true);
        assert!(!root.join(format!("batch-{}/", failed_batch + 1)).exists());
        if first_batch_failure {
            assert!(!project.0.join("attempts.jsonl").exists());
        } else {
            assert_eq!(
                records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
                2
            );
        }
    }
}

#[test]
fn measurement_override_only_relocates_measurement_outputs() {
    let project = Project::new();
    project.config(1);
    let output = project.run_extra("1", &["--measurements", "alternate-measurements"]);
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = records(&output.stdout);
    let path = PathBuf::from(rows[0]["path"].as_str().unwrap());
    assert!(path.starts_with(project.0.join("results/asb-executions")));
    let plan: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let measurements = PathBuf::from(plan["measurement_root"].as_str().unwrap());
    assert!(measurements.starts_with(project.0.join("alternate-measurements")));
    assert!(measurements.join("batch-1/measurements/run.json").is_file());
    assert!(!measurements.join("batch-1/workspace").exists());
    assert!(
        path.parent()
            .unwrap()
            .join("batch-1/workspace/src/parser.py")
            .is_file()
    );
}

#[test]
fn sparse_legacy_replica_64_is_resolved_bound_and_replayed() {
    let project = Project::new();
    project.config(2);
    fs::rename(
        project.0.join("input.jsonl"),
        project.0.join("input-concurrency-64.jsonl"),
    )
    .unwrap();
    let output = project.run("1");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = records(&output.stdout);
    let plan: Value =
        serde_json::from_slice(&fs::read(rows[0]["path"].as_str().unwrap()).unwrap()).unwrap();
    assert!(
        plan["replay_inputs"][0]["source"]
            .as_str()
            .unwrap()
            .ends_with("input-concurrency-64.jsonl")
    );
    assert_eq!(plan["slots"][0]["replay_input"], 0);
    assert_eq!(plan["slots"][1]["replay_input"], 0);
    assert_eq!(rows.len(), 3);
}

#[test]
fn closed_output_stops_admission_after_joining_the_current_batch() {
    closed_output_stops_admission(false);
}

#[test]
fn closed_output_with_grader_failure_stops_after_joining_the_current_batch() {
    closed_output_stops_admission(true);
}

fn closed_output_stops_admission(grader_failure: bool) {
    use std::io::BufRead;
    use std::process::Stdio;
    let project = Project::new();
    project.config(2);
    if grader_failure {
        fs::write(
            project.0.join("grader-error-first"),
            "inject grader failure",
        )
        .unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_asb"))
        .current_dir(&project.0)
        .isolated_env()
        .env("HOME", &project.0)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args([
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "input.jsonl",
            "--repeat",
            "3",
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut source = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    source.read_line(&mut line).unwrap();
    let event: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(event["type"], "execution-plan");
    drop(source);
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let path = PathBuf::from(event["path"].as_str().unwrap());
    let root = path.parent().unwrap();
    let finished: Value =
        serde_json::from_slice(&fs::read(root.join("batch-1/finished.json")).unwrap()).unwrap();
    assert_eq!(finished["stopped"], true);
    assert!(!root.join("batch-2").exists());
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        2
    );
    assert_eq!(
        fs::read_dir(root.join("batch-1/outcomes")).unwrap().count(),
        2
    );
    if grader_failure {
        let summaries: Vec<Value> = fs::read_dir(root.join("batch-1/outcomes"))
            .unwrap()
            .map(|entry| {
                let link: Value =
                    serde_json::from_slice(&fs::read(entry.unwrap().path()).unwrap()).unwrap();
                let outcome: Value = serde_json::from_slice(
                    &fs::read(root.join(link["path"].as_str().unwrap())).unwrap(),
                )
                .unwrap();
                outcome["summary"].clone()
            })
            .collect();
        assert_eq!(
            summaries
                .iter()
                .filter(|row| row["failure_kind"] == "grader_error"
                    && row["workload_success"].is_null())
                .count(),
            1
        );
    }
}

#[test]
fn explicit_one_is_fresh_and_invalid_products_publish_nothing() {
    let project = Project::new();
    project.config(1);
    let output = project.run("1");
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        records(&fs::read(project.0.join("attempts.jsonl")).unwrap()).len(),
        1
    );
    assert_eq!(
        fs::read_dir(project.0.join("workspace")).unwrap().count(),
        1
    );
    let invalid = Project::new();
    invalid.config(64);
    for repeat in ["0", "1024", "18446744073709551615"] {
        assert!(!invalid.run(repeat).status.success());
        assert!(!invalid.0.join("results").exists());
        assert!(!invalid.0.join("attempts.jsonl").exists());
    }
}
