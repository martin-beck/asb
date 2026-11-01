//! Cross-feature actual runner attempts: bundled tape and durable publication.
use super::*;
use crate::evidence::{
    FaultPoint,
    tests::{Scope, WriteAction},
};
use serde_json::{Value, json};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::Ordering;
#[path = "../support/recording_provider.rs"]
mod recording_provider;
use recording_provider::RecordingProvider;

#[test]
fn actual_bundled_record_replay_publication_faults_preserve_independent_evidence() {
    isolated_full_entry_cases(&[
        "bundle-publication-record-true-log-short",
        "bundle-publication-record-false-log-short",
        "bundle-publication-record-native-log-sync",
        "bundle-publication-record-true-run-short",
        "bundle-publication-record-false-summary-sync",
        "bundle-publication-record-true-commit-failure-log-short",
        "bundle-publication-record-false-client-commit-failure-summary-sync",
        "bundle-publication-replay-true-replace-run-short",
        "bundle-publication-replay-true-replace-log-sync",
    ]);
}

#[test]
fn actual_bundled_artifact_handoff_survives_commit_and_publication_failures() {
    isolated_full_entry_cases(&[
        "bundle-publication-artifact-record-true-log-short",
        "bundle-publication-artifact-record-false-run-short",
        "bundle-publication-artifact-record-native-log-sync",
        "bundle-publication-artifact-record-true-commit-failure-log-short",
        "bundle-publication-artifact-record-false-client-commit-failure-summary-sync",
        "bundle-publication-artifact-replay-true-replace-run-short",
    ]);
}

fn trajectory(answer: &str) -> Value {
    json!({"trajectory_format":"mini-swe-agent-1.1",
        "info":{"exit_status":"Submitted","submission":answer,"model_stats":{"api_calls":4},"private_note":"fixture-private-artifact-note"},
        "messages":[{"role":"assistant","content":answer,
            "extra":{"response":{"usage":{"prompt_tokens":13,"completion_tokens":21}}}}]})
}

const OLD_LOG: &[u8] = b"{\"previous\":\"bundle log sentinel\"}\n";
const OLD_RUN: &[u8] = b"{\"previous\":\"bundle measurement sentinel\"}";

/// The actual repeated runner announces its fresh campaign before admission.
/// Arm an existing exact-path private fault there, without a production hook.
struct Output {
    bytes: Vec<u8>,
    parsed: usize,
    case: Option<String>,
    campaign: Option<PathBuf>,
    fault: Option<Scope>,
}
impl Output {
    fn new(case: Option<&str>) -> Self {
        Self {
            bytes: Vec::new(),
            parsed: 0,
            case: case.map(str::to_owned),
            campaign: None,
            fault: None,
        }
    }
    fn rows(&self) -> Vec<Value> {
        std::str::from_utf8(&self.bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        while let Some(end) = self.bytes[self.parsed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = self.parsed + end;
            let row: Value = serde_json::from_slice(&self.bytes[self.parsed..end]).unwrap();
            self.parsed = end + 1;
            if row["type"] != "execution-plan" {
                continue;
            }
            assert!(
                self.campaign.is_none(),
                "one plan per run, no implicit retry"
            );
            let campaign = PathBuf::from(row["path"].as_str().unwrap())
                .parent()
                .unwrap()
                .to_path_buf();
            if let Some(case) = &self.case {
                let batch = campaign.join("batch-1");
                let (path, point, writes) = if case.contains("log-") {
                    let path = batch.join("run.jsonl");
                    (
                        path,
                        case.contains("sync").then_some(FaultPoint::DirectorySync),
                        if case.contains("short") {
                            vec![
                                WriteAction::Short(OLD_LOG.len()),
                                WriteAction::Short(5),
                                WriteAction::NoSpace,
                            ]
                        } else {
                            vec![]
                        },
                    )
                } else if case.contains("run-short") {
                    let path = batch.join("measurements/run.json");
                    (
                        path,
                        None,
                        vec![WriteAction::Short(5), WriteAction::NoSpace],
                    )
                } else {
                    (
                        batch.join("measurements/summary.json"),
                        Some(FaultPoint::DirectorySync),
                        vec![],
                    )
                };
                self.fault = Some(Scope::new(path, point, writes));
            }
            self.campaign = Some(campaign);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn last_log(path: &Path) -> Value {
    serde_json::from_slice(
        fs::read(path)
            .unwrap()
            .split(|byte| *byte == b'\n')
            .rfind(|line| !line.is_empty())
            .unwrap(),
    )
    .unwrap()
}

pub(super) fn run(case: &str) {
    if std::env::var_os("ASB_BUNDLE_PUBLICATION_CHILD").is_some() {
        run_child(case);
        return;
    }
    use support::IsolatedEnvironment;
    let provider = RecordingProvider::new();
    let executable = std::env::current_exe().unwrap();
    let runtime = executable.parent().unwrap().parent().unwrap().join("asb");
    let child = Command::new(&executable)
        .args(["--exact", "runner::phases::tests::full_runner_entry_keeps_real_fixture_grade_and_primary_error_after_lifecycle_failure", "--nocapture"])
        .isolated_env()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("ASB_RUNTIME_EXECUTABLE", runtime)
        .env("ASB_RUNNER_ENTRY_TEST_CASE", case)
        .env("ASB_BUNDLE_PUBLICATION_CHILD", "1")
        .env("OPENROUTER_API_KEY", "fixture-only-key")
        .env("OPENROUTER_BASE_URL", format!("{}/v1", provider.endpoint))
        .env("OPENROUTER_MODELS_URL", format!("{}/models", provider.endpoint))
        .output().unwrap();
    assert!(
        child.status.success(),
        "{case}: {}\n{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "one synthetic recording request; replay must not issue inference"
    );
    println!("qualified combined offline case: {case}");
}

fn run_child(case: &str) {
    let fixture = Fixture::new();
    let root = fixture.0.canonicalize().unwrap();
    fs::create_dir(root.join(".asb")).unwrap();
    fs::write(
        root.join(".asb/project.toml"),
        "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
    )
    .unwrap();
    fs::create_dir(root.join("workloads")).unwrap();
    workloads::populate(&root.join("workloads")).unwrap();
    let artifact = case.contains("artifact-");
    let agent = root.join(if artifact {
        "tools/mini-swe/bin/mini"
    } else {
        "tools/pi/bin/pi"
    });
    fs::create_dir_all(agent.parent().unwrap()).unwrap();
    let counter = root.join("agent-executions");
    let cassette = root.join("bundle.zip");
    let native = case.contains("native");
    let false_grade = case.contains("false");
    let replay = case.contains("replay-");
    let node = crate::tools::host_executable(Path::new("node")).unwrap();
    let body = json!({"model":"fixture","tools":[{"type":"function","function":{"name":"fixture","parameters":{"type":"object"}}}],"messages":[]});
    let answer = if false_grade {
        "incorrect"
    } else {
        "src/cli.py src/registry.py lookup"
    };
    // In artifact cases stdout deliberately disagrees with the frozen answer.
    // True/false grades must come from the owning adapter's frozen trajectory.
    let transcript_answer = if artifact {
        if false_grade {
            "src/cli.py src/registry.py lookup"
        } else {
            "incorrect"
        }
    } else {
        answer
    };
    let response = json!({"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"text","text":transcript_answer}]}]});
    let artifact_script = if artifact {
        format!(
            "const p=require('node:path');const oi=process.argv.indexOf('--output');const out=process.argv[oi+1];const owned=p.resolve(process.cwd()+'.asb-agent-runtime','runs')+p.sep;if(oi<0||!out||!p.resolve(out).startsWith(owned)||!out.endsWith(p.join('mini-swe','trajectory.json')))throw Error('missing private attempt output');fs.writeFileSync(out,JSON.stringify({}));console.log(JSON.stringify({{usage:{{prompt_tokens:499,completion_tokens:500,total_tokens:999}}}}));",
            trajectory(answer)
        )
    } else {
        String::new()
    };
    let counter_js = serde_json::to_string(&counter).unwrap();
    let cassette_js = serde_json::to_string(&cassette).unwrap();
    let partial_js =
        serde_json::to_string(&root.join(".bundle.zip.partial/track-1.jsonl")).unwrap();
    let seed = if case.contains("log-") {
        format!(
            "fs.writeFileSync(require('node:path').join(require('node:path').dirname(process.cwd()),'run.jsonl'),{});",
            serde_json::to_string(std::str::from_utf8(OLD_LOG).unwrap()).unwrap()
        )
    } else if case.contains("run-short") {
        format!(
            "fs.writeFileSync(require('node:path').join(require('node:path').dirname(process.cwd()),'measurements/run.json'),{});",
            serde_json::to_string(std::str::from_utf8(OLD_RUN).unwrap()).unwrap()
        )
    } else {
        String::new()
    };
    let script = |replace: bool, block: bool| {
        format!(
            "#!{}\nconst fs=require('node:fs');fs.appendFileSync({counter_js},'called\\n');{artifact_script}{}fetch(process.env.OPENAI_BASE_URL+'/chat/completions',{{method:'POST',headers:{{'content-type':'application/json'}},body:JSON.stringify({body})}}).then(async r=>{{if(!r.ok)throw Error('fixture request failed');await r.text();{}{}console.log(JSON.stringify({response}));process.exitCode={};}}).catch(e=>{{console.error(e);process.exitCode=9;}});\n",
            node.display(),
            if replace || !replay {
                seed.as_str()
            } else {
                ""
            },
            if replace {
                format!("fs.writeFileSync({cassette_js},'source replaced after private staging');")
            } else {
                String::new()
            },
            if block {
                format!("fs.writeFileSync({partial_js},'preserved blocking stage');")
            } else {
                String::new()
            },
            if case.contains("client") { 7 } else { 0 }
        )
    };
    fs::write(&agent, script(false, case.contains("commit-failure"))).unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
    let mut config: BenchmarkConfig = toml::from_str("agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='repository-navigation'\nworkspace='workspace'\nlog_file='run.jsonl'\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=[]\n").unwrap();
    if artifact {
        config.agent = "mini-swe".into();
    }
    let config_path = root.join("benchmark.toml");
    fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    let calls = Rc::new(RefCell::new(Vec::new()));
    if replay {
        let scope = entry_test_support::Scope::recording(&root, calls.clone(), false);
        let mut baseline = Output::new(None);
        super::super::super::repeated::run(
            &config_path,
            config.clone(),
            1,
            RunOptions {
                json: true,
                mode: Some(tape::Mode::Record),
                cassette: Some(cassette.to_str().unwrap()),
                ..Default::default()
            },
            &mut baseline,
            &Mutex::new(BTreeSet::new()),
        )
        .unwrap();
        assert_eq!(
            calls.borrow().as_slice(),
            ["evaluate", "finish", "provider-finish"]
        );
        drop(scope);
        calls.borrow_mut().clear();
        fs::write(&counter, "").unwrap();
        fs::write(&agent, script(true, false)).unwrap();
    }
    let _scope = if replay {
        entry_test_support::Scope::provider(&root, calls.clone(), false, false)
    } else {
        entry_test_support::Scope::recording(&root, calls.clone(), native)
    };
    let mut output = Output::new(Some(case));
    let result = super::super::super::repeated::run(
        &config_path,
        config,
        2,
        RunOptions {
            json: true,
            mode: Some(if replay {
                tape::Mode::Replay
            } else {
                tape::Mode::Record
            }),
            cassette: Some(cassette.to_str().unwrap()),
            track: replay.then_some(crate::cassette::Selector::Id(1)),
            replay_seed: replay.then_some(73),
            ..Default::default()
        },
        &mut output,
        &Mutex::new(BTreeSet::new()),
    );
    check(case, &root, &output, &calls, result);
}

fn check(
    case: &str,
    root: &Path,
    output: &Output,
    calls: &Rc<RefCell<Vec<&'static str>>>,
    result: Result<(), Error>,
) {
    let error = result.unwrap_err().to_string();
    let publication = if case.contains("log-") {
        "run log publication failed"
    } else {
        "measurement finalization failed"
    };
    assert!(error.contains(publication), "{case}: {error}");
    assert!(
        output.fault.as_ref().unwrap().hits() > 0,
        "publication fault did not fire"
    );
    assert_eq!(
        calls.borrow().as_slice(),
        ["evaluate", "finish", "provider-finish"]
    );
    assert_eq!(
        fs::read_to_string(root.join("agent-executions")).unwrap(),
        "called\n"
    );
    let campaign = output.campaign.as_ref().unwrap();
    let batch = campaign.join("batch-1");
    assert!(
        !campaign.join("batch-2").exists(),
        "evidence failure must stop later admission"
    );
    assert_eq!(read(&campaign.join("finished.json"))["stopped"], true);
    assert_eq!(fs::read_dir(batch.join("outcomes")).unwrap().count(), 0);
    assert_eq!(
        read(&batch.join("finished.json"))["completed_executions"],
        0
    );
    assert!(!batch.join(".asb-completions").exists());
    let rows = output.rows();
    let summaries: Vec<_> = rows
        .iter()
        .filter(|row| row.get("task").is_some())
        .collect();
    assert_eq!(summaries.len(), 1, "{rows:?}");
    let shown = summaries[0];
    let measured = if case.contains("run-short") {
        assert_eq!(
            fs::read(batch.join("measurements/run.json")).unwrap(),
            OLD_RUN
        );
        last_log(&batch.join("run.jsonl"))
    } else {
        read(&batch.join("measurements/run.json"))
    };
    if case.contains("log-short") {
        assert_eq!(fs::read(batch.join("run.jsonl")).unwrap(), OLD_LOG);
    } else {
        let log = last_log(&batch.join("run.jsonl"));
        for key in [
            "task",
            "execution",
            "benchmark_outcome",
            "token_usage",
            "recording",
        ] {
            assert_eq!(log[key], shown[key], "log {key}");
        }
    }
    for key in [
        "task",
        "execution",
        "recording",
        "benchmark_outcome",
        "token_usage",
        "execution_plan_fingerprint",
        "execution_cohort",
        "workload_resource_disposition",
        "workload_artifacts",
    ] {
        assert_eq!(measured[key], shown[key], "measurement {key}");
    }
    let summary = read(&batch.join("measurements/summary.json"));
    for key in [
        "task",
        "execution",
        "recording",
        "benchmark_outcome",
        "token_usage",
        "execution_plan_fingerprint",
        "execution_cohort",
    ] {
        assert_eq!(summary["runs"][0][key], shown[key], "summary {key}");
    }
    // Validated tape usage remains authoritative even though the independent
    // frozen trajectory contains 34 tokens and stdout claims 999.
    let tokens = 5;
    assert_eq!(shown["token_usage"]["total_tokens"], tokens);
    if case.contains("artifact-") {
        assert_eq!(shown["agent"], "mini-swe");
        for record in [shown, &measured, &summary["runs"][0]] {
            assert_eq!(record["steps"], 4);
            assert!(
                record["token_usage"]["source"]
                    .as_str()
                    .unwrap()
                    .contains("cassette")
            );
        }
        let attempts: Vec<_> = fs::read_dir(
            crate::runner::replica::runtime_path(&batch.join("workspace")).join("runs"),
        )
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
        assert_eq!(attempts.len(), 1, "one admitted client/attempt");
        let frozen = attempts[0].join("mini-swe/frozen-trajectory.json");
        let answer = if case.contains("false") {
            "incorrect"
        } else {
            "src/cli.py src/registry.py lookup"
        };
        assert_eq!(
            fs::read(&frozen).unwrap(),
            serde_json::to_vec(&trajectory(answer)).unwrap(),
            "exact frozen bytes remain recoverable, never reconstruct from the current pathname"
        );
        let document = read(&frozen);
        assert_eq!(document["messages"][0]["content"], answer);
        assert_eq!(document["info"]["model_stats"]["api_calls"], 4);
        assert_eq!(
            document["messages"][0]["extra"]["response"]["usage"]["prompt_tokens"],
            13
        );
        assert_eq!(
            document["messages"][0]["extra"]["response"]["usage"]["completion_tokens"],
            21
        );
        assert_eq!(
            fs::metadata(&frozen).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!attempts[0].join("mini-swe/trajectory.json").exists());
        assert!(error.contains("retained frozen agent evidence"), "{error}");
        assert!(
            error.contains(&format!(
                "{}/mini-swe/frozen-trajectory.json",
                attempts[0].file_name().unwrap().to_str().unwrap()
            )),
            "{error}"
        );
        assert!(
            !error.contains("fixture-private-artifact-note"),
            "diagnostics must not expose raw artifact contents: {error}"
        );
    }
    assert_eq!(shown["process_success"], !case.contains("client"));
    assert_eq!(
        shown["benchmark_outcome"]["evaluation"]["status"],
        "completed"
    );
    assert_eq!(shown["benchmark_outcome"]["cleanup_confirmed"], true);
    if case.contains("native") {
        assert!(shown["workload_success"].is_null());
        assert!(shown["benchmark_outcome"]["evaluation"]["resolution"].is_null());
        assert_eq!(
            shown["benchmark_outcome"]["evaluation"]["rewards"]["accuracy"],
            0.5
        );
    } else {
        assert_eq!(shown["workload_success"], !case.contains("false"));
    }
    for name in ["lifecycle.json", "components.json"] {
        let _ = read(&batch.join("measurements").join(name));
    }
    if case.contains("sync") {
        assert!(
            error.contains("was published, but durability is unconfirmed"),
            "{error}"
        );
    }
    if case.contains("false") {
        let primary = measured["failure_reason"].as_str().unwrap();
        assert!(
            error.find(primary).unwrap() < error.find(publication).unwrap(),
            "original benchmark/client failure must remain primary: {error}"
        );
    }
    if case.contains("replay-") {
        let selection = rows
            .iter()
            .find(|row| row["type"] == "replay-selection")
            .unwrap();
        assert_eq!(selection["selection"]["tracks"], json!([1, 1]));
        assert_eq!(selection["selection"]["seed"], 73);
        let log = last_log(&batch.join("run.jsonl"));
        // All current result representations retain the actual frozen choice.
        for record in [shown, &log, &measured] {
            assert_eq!(record["execution"]["replay_track"], 1);
            assert_eq!(record["execution"]["replay_seed"], 73);
            assert_eq!(
                record["execution"]["replay_algorithm"],
                selection["selection"]["algorithm"]
            );
        }
        let plan = read(&campaign.join("plan.json"));
        assert_eq!(plan["replay_selection"], selection["selection"]);
        assert_eq!(
            fs::read_to_string(root.join("bundle.zip")).unwrap(),
            "source replaced after private staging"
        );
        assert!(shown.get("recording").is_none());
    } else if case.contains("commit-failure") {
        assert!(error.contains("recording"), "{error}");
        assert_eq!(measured["status"], "failed");
        let expected_kind = if case.contains("client") {
            "agent_failure"
        } else {
            "recording_error"
        };
        assert_eq!(measured["failure_kind"], expected_kind);
        assert_eq!(shown["failure_kind"], expected_kind);
        assert!(measured["failure_reason"].as_str().unwrap().contains(
            if case.contains("client") {
                "agent"
            } else {
                "recording"
            }
        ));
        assert!(shown.get("recording").is_none());
        assert!(!root.join("bundle.zip").exists());
        assert_eq!(
            fs::read_to_string(root.join(".bundle.zip.partial/track-1.jsonl")).unwrap(),
            "preserved blocking stage"
        );
        assert!(batch.join("capture.jsonl").is_file());
        assert_eq!(
            read(&campaign.join("finished.json"))["cassette_publication"]["status"],
            "failed"
        );
    } else {
        let evidence: super::super::super::recording::Evidence =
            serde_json::from_value(shown["recording"].clone()).unwrap();
        evidence
            .track
            .verify(crate::cassette::Limits::default())
            .unwrap();
        let association = read(&root.join(".bundle.zip.partial/association-1.json"));
        assert_eq!(association["schema_version"], 1);
        assert_eq!(
            association["evidence_sha256"],
            crate::cassette::digest(&serde_json::to_vec(&evidence).unwrap())
        );
        assert_eq!(evidence.attempt_id, shown["task"]["attempt_id"]);
        assert_eq!(evidence.track.track.id, shown["execution"]["slot"]);
        assert_eq!(
            evidence.track.track.process_success,
            shown["process_success"].as_bool()
        );
        assert_eq!(
            evidence.track.track.workload_success,
            shown["workload_success"].as_bool()
        );
        assert_eq!(evidence.track.track.statistics.tokens, Some(tokens));
        if case.contains("artifact-") {
            assert_eq!(evidence.track.track.statistics.steps, Some(4));
        }
        assert_eq!(evidence.track.track.statistics.requests, Some(1));
        assert!(evidence.track.track.complete);
        let raw = fs::read(&evidence.producer).unwrap();
        assert_eq!(raw.len() as u64, evidence.track.track.bytes);
        assert_eq!(crate::cassette::digest(&raw), evidence.track.track.sha256);
        let finished = read(&campaign.join("finished.json"));
        assert_eq!(finished["cassette_publication"]["status"], "published");
        assert_eq!(
            finished["cassette_publication"]["cleanup_complete"], false,
            "publication fault must not claim raw producer cleanup"
        );
    }
}
