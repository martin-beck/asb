//! Offline end-to-end sweep replay using a fake agent and a real loopback tape.

#![cfg(unix)]

mod support;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};
use support::IsolatedEnvironment;

fn asb(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_asb"));
    command
        .current_dir(root)
        .args(args)
        .isolated_env()
        .env("HOME", root)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());
    command.output().expect("ASB starts")
}

#[test]
fn sweep_replays_one_recording_eight_times_without_a_live_bridge() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("asb-sweep-replay-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let init = asb(&root, &["init"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let agent_dir = root.join(".asb-tools/pi/bin");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let agent = agent_dir.join("pi");
    std::fs::write(&agent, r##"#!/usr/bin/env node
const fs = require('fs');
fs.appendFileSync(process.env.HOME + '/agent-launches', 'started\n');
const conf = JSON.parse(fs.readFileSync(process.env.PI_CODING_AGENT_DIR + '/models.json'));
const url = conf.providers.asb.baseUrl;
if (conf.providers.asb.api !== 'openai-responses') process.exit(4);
if (!/^http:\/\/127\.0\.0\.1:\d+\/v1$/.test(url || '')) process.exit(3);
const body = {model: 'test-model', input: 'example prompt', tools: [{type: 'function'}]};
fetch(url + '/responses', {method:'POST', headers:{'content-type':'application/json'}, body:JSON.stringify(body)})
  .then(async r => { if (!r.ok) throw new Error(await r.text()); fs.writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n"); console.log('{"status":"ok"}'); })
  .catch(e => { console.error(e); process.exitCode = 1; });
"##).unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join("benchmark.toml"),
        "agent='pi'\nworkload='bug-fix'\nprovider='cli2key'\nmodel='test-model'\nconcurrency=8\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=[]\n",
    )
    .unwrap();
    std::fs::write(root.join("sweep.toml"), "configs=['benchmark.toml']\n").unwrap();
    for index in 1..=8 {
        let workspace = root.join(format!(
            "sweep-results/000-benchmark/workspace-concurrency-{index}/src"
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("parser.py"),
            "def parse_path(value: str) -> list[str]:\n    return value.split('/')\n",
        )
        .unwrap();
    }
    let cassette_dir = root.join("tapes/000-benchmark/cassettes");
    std::fs::create_dir_all(&cassette_dir).unwrap();
    let cassette = cassette_dir.join("benchmark-pi-bug-fix-test-model.jsonl");
    let body = r#"{"model":"test-model","input":"example prompt","tools":[{"type":"function"}]}"#;
    let interaction = serde_json::json!({"request":{"method":"POST","url":"/v1/responses","body":body},"response":{"status":200,"headers":{"content-type":"application/json"},"body":"{}"}});
    std::fs::write(&cassette, format!("{interaction}\n")).unwrap();
    let result = asb(
        &root,
        &[
            "sweep",
            "sweep.toml",
            "--replay",
            "--cassette-root",
            "tapes",
        ],
    );
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let progress = String::from_utf8_lossy(&result.stdout);
    assert_eq!(
        progress.matches("Starting benchmark (").count(),
        1,
        "{progress}"
    );
    assert!(
        progress.contains(
            "(1/1; 100% reached): benchmark.toml — pi, cli2key, test-model, bug-fix, trial default (concurrency 8)"
        ),
        "{progress}"
    );
    for index in 1..=8 {
        let log = root.join(format!(
            "sweep-results/000-benchmark/run-concurrency-{index}.log"
        ));
        let value: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(log).unwrap().trim()).unwrap();
        assert_eq!(value["status"], "completed");
        let receipt: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join(format!(
                "sweep-results/.checkpoint/replicas/000000/{index:03}.json"
            )))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            receipt["replica"],
            serde_json::json!({"index":index,"total":8})
        );
        assert_eq!(receipt["scope"], "completed-replica-declared-evidence");
        assert_eq!(receipt["fingerprint"]["digest"].as_str().unwrap().len(), 64);
    }
    let comparison: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("results/sweep-comparison/comparison.json")).unwrap(),
    )
    .unwrap();
    let input_seal = root.join("sweep-results/.checkpoint/inputs.json");
    let before_inputs = std::fs::read(&input_seal).unwrap();
    let before_log =
        std::fs::read(root.join("sweep-results/000-benchmark/run-concurrency-1.log")).unwrap();
    let launches = std::fs::read(root.join("agent-launches")).unwrap();
    assert_eq!(launches.iter().filter(|byte| **byte == b'\n').count(), 8);
    let resumed = asb(
        &root,
        &[
            "sweep",
            "sweep.toml",
            "--replay",
            "--cassette-root",
            "tapes",
            "--resume",
            "--json",
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let summaries: Vec<serde_json::Value> = String::from_utf8(resumed.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(summaries.len(), 8);
    assert!(
        summaries
            .iter()
            .all(|summary| summary["reused"] == true && summary["workload_success"] == true)
    );
    assert_eq!(
        std::fs::read(root.join("agent-launches")).unwrap(),
        launches
    );
    assert_eq!(
        std::fs::read(root.join("sweep-results/000-benchmark/run-concurrency-1.log")).unwrap(),
        before_log
    );
    assert!(
        !root.join("sweep-history").is_dir()
            || std::fs::read_dir(root.join("sweep-history"))
                .unwrap()
                .count()
                == 1
    );
    // A changed selected executable must reject continuation before replacing
    // the original seal, completed results, logs or any prior report.
    let original_agent = std::fs::read(&agent).unwrap();
    let mut changed_agent = original_agent.clone();
    changed_agent.extend_from_slice(b"\n// changed installed input\n");
    std::fs::write(&agent, changed_agent).unwrap();
    let stable_seal = std::fs::read(&input_seal).unwrap();
    let rejected = asb(
        &root,
        &[
            "sweep",
            "sweep.toml",
            "--replay",
            "--cassette-root",
            "tapes",
            "--resume",
        ],
    );
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("inputs changed"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert_eq!(std::fs::read(&input_seal).unwrap(), stable_seal);
    assert_eq!(
        std::fs::read(root.join("agent-launches")).unwrap(),
        launches
    );
    std::fs::write(&agent, original_agent).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&before_inputs).unwrap()["initial"],
        serde_json::from_slice::<serde_json::Value>(&stable_seal).unwrap()["initial"]
    );
    // Recover the window after all per-replica seals were written but before
    // the coordinator published its final group receipt, without new attempts.
    let receipt = root.join("sweep-results/.checkpoint/completed/000000.json");
    let saved_receipt = root.join("preserved-receipt.json");
    std::fs::rename(&receipt, &saved_receipt).unwrap();
    let recovered = asb(
        &root,
        &[
            "sweep",
            "sweep.toml",
            "--replay",
            "--cassette-root",
            "tapes",
            "--resume",
            "--json",
        ],
    );
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(
        std::fs::read(root.join("agent-launches")).unwrap(),
        launches
    );
    assert!(
        !receipt.exists(),
        "validation does not republish a missing group receipt"
    );
    let replica_receipt = root.join("sweep-results/.checkpoint/replicas/000000/001.json");
    let saved_replica = root.join("preserved-replica-receipt.json");
    std::fs::rename(&replica_receipt, &saved_replica).unwrap();
    let stable_seal = std::fs::read(&input_seal).unwrap();
    // One missing replica seal must not turn this group into a fresh retry.
    let rejected = asb(
        &root,
        &[
            "sweep",
            "sweep.toml",
            "--replay",
            "--cassette-root",
            "tapes",
            "--resume",
        ],
    );
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("interrupted replica group"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(!receipt.exists());
    assert_eq!(std::fs::read(&input_seal).unwrap(), stable_seal);
    assert_eq!(
        std::fs::read(root.join("agent-launches")).unwrap(),
        launches
    );
    std::fs::rename(saved_receipt, &receipt).unwrap();
    std::fs::rename(saved_replica, &replica_receipt).unwrap();
    // A pending initial seal left by a stopped owner still requires exact
    // current inputs and complete graded groups, not a previous success exit.
    let mut pending: serde_json::Value = serde_json::from_slice(&stable_seal).unwrap();
    pending["status"] = "pending".into();
    pending["final"] = serde_json::Value::Null;
    std::fs::write(&input_seal, serde_json::to_vec(&pending).unwrap()).unwrap();
    let resumed = asb(
        &root,
        &[
            "sweep",
            "sweep.toml",
            "--replay",
            "--cassette-root",
            "tapes",
            "--resume",
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(
        std::fs::read(root.join("agent-launches")).unwrap(),
        launches
    );
    assert!(String::from_utf8_lossy(&resumed.stdout).contains("Reused completed task"));
    let key = root.join("sweep-results/.checkpoint/key");
    let preserved_key = root.join("preserved-checkpoint-key");
    std::fs::rename(&key, &preserved_key).unwrap();
    let rejected = asb(
        &root,
        &[
            "sweep",
            "sweep.toml",
            "--replay",
            "--cassette-root",
            "tapes",
            "--resume",
        ],
    );
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("original private input checkpoint is missing")
    );
    assert!(
        !key.exists(),
        "resume must never replace a missing historical key"
    );
    assert_eq!(
        std::fs::read(root.join("agent-launches")).unwrap(),
        launches
    );
    std::fs::rename(preserved_key, key).unwrap();
    let benchmarks = comparison["benchmarks"].as_array().unwrap();
    assert_eq!(benchmarks.len(), 8);
    for benchmark in benchmarks {
        assert_eq!(benchmark["status"], "completed", "{benchmark}");
        assert!(
            benchmark["metrics"]
                .as_object()
                .is_some_and(|m| !m.is_empty()),
            "{benchmark}"
        );
    }
    assert_eq!(
        std::fs::read_dir(&cassette_dir).unwrap().count(),
        1,
        "tape session files were cleaned up"
    );
    let default_dir = root.join("sweep-results/000-benchmark/cassettes");
    std::fs::create_dir_all(&default_dir).unwrap();
    std::fs::copy(&cassette, default_dir.join(cassette.file_name().unwrap())).unwrap();
    let result = asb(&root, &["sweep", "sweep.toml", "--replay"]);
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let restored = default_dir.join(cassette.file_name().unwrap());
    assert!(
        restored.is_file(),
        "default recording must survive fresh replay output attempts"
    );
    use std::os::unix::fs::MetadataExt;
    let recording_inode = std::fs::metadata(&restored).unwrap().ino();
    for extra in [vec![], vec!["--cassette-root", "sweep-results"], vec![]] {
        let mut args = vec!["sweep", "sweep.toml", "--replay"];
        args.extend(extra);
        let replay = asb(&root, &args);
        assert!(
            replay.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&replay.stdout),
            String::from_utf8_lossy(&replay.stderr)
        );
        assert_eq!(
            std::fs::metadata(&restored).unwrap().ino(),
            recording_inode,
            "replays should reuse, not duplicate recording payloads"
        );
        let current: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("results/sweep-comparison/comparison.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(current["selection_plan"]["execution_mode"], "replay");
        assert_eq!(current["evaluation_coverage"]["evaluated_attempts"], 8);
        let seal: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("sweep-results/.checkpoint/inputs.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(seal["status"], "stable", "{seal}");
        let source = seal["roots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|input| input["name"] == "replay-source-0")
            .unwrap();
        let source = Path::new(source["path"].as_str().unwrap());
        assert!(source.starts_with(root.join("sweep-history")));
        assert!(source.ends_with("sweep-results/000-benchmark/cassettes"));
        assert!(source.join(cassette.file_name().unwrap()).is_file());
        assert!(
            current["benchmarks"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["metrics"]["success_rate"] == 1.0)
        );
        let launches = std::fs::read(root.join("agent-launches")).unwrap();
        let mut continuation_args = args.clone();
        continuation_args.push("--resume");
        let resumed = asb(&root, &continuation_args);
        assert!(
            resumed.status.success(),
            "{}",
            String::from_utf8_lossy(&resumed.stderr)
        );
        assert_eq!(
            std::fs::read(root.join("agent-launches")).unwrap(),
            launches
        );
        assert_eq!(std::fs::metadata(&restored).unwrap().ino(), recording_inode);
    }
    std::fs::remove_dir_all(root).unwrap();
}
