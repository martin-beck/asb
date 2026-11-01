//! Offline runner admission and owned-child stream handling.
use super::*;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "asb-runner-boundaries-{}",
            workloads::TaskIdentity::local("boundary", None).attempt_id
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn config(&self) -> PathBuf {
        let path = self.0.join("benchmark.toml");
        std::fs::write(
            &path,
            "agent='fixture'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\n",
        )
        .unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn admission_rejects_bad_replica_reuse_and_cohort_before_any_execution() {
    let _disk = crate::disk_space::test_available_mib(100_000);
    let root = Fixture::new();
    let path = root.config();
    let options = RunOptions {
        replica: replica::Identity { index: 0, total: 1 },
        ..Default::default()
    };
    assert!(
        run_with_tape(path.to_str().unwrap(), options, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("invalid benchmark replica identity")
    );
    let reused = std::collections::BTreeMap::from([(2, serde_json::json!({}))]);
    let options = RunOptions {
        reused: Some(&reused),
        ..Default::default()
    };
    assert!(
        run_with_tape(path.to_str().unwrap(), options, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("outside the selected population")
    );
    let cohort = measurements::ExecutionCohort {
        id: "fixture".into(),
        selected_replicas: 1,
        admitted_slots: Vec::new(),
        dispatch: None,
    };
    let options = RunOptions {
        execution_cohort: Some(&cohort),
        ..Default::default()
    };
    let config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        run_mode(
            &path,
            config,
            options,
            &mut Vec::new(),
            &Mutex::new(BTreeSet::new())
        )
        .unwrap_err()
        .to_string()
        .contains("does not admit the selected replica")
    );
    assert_eq!(
        std::fs::read_dir(&root.0).unwrap().count(),
        1,
        "no workspace or artifact may be created"
    );
}

#[test]
fn single_reused_result_renders_without_provider_project_or_agent_startup() {
    let _disk = crate::disk_space::test_available_mib(100_000);
    let root = Fixture::new();
    let path = root.config();
    let summary = serde_json::json!({"task":{"task_id":"retained"},"process_success":true,"workload_success":true,"token_usage":{"total_tokens":123}});
    let reused = std::collections::BTreeMap::from([(1, summary.clone())]);
    for json in [false, true] {
        let options = RunOptions {
            json,
            reused: Some(&reused),
            ..Default::default()
        };
        let mut output = Vec::new();
        run_with_tape(path.to_str().unwrap(), options, &mut output).unwrap();
        if json {
            let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(value["reused"], true);
            assert_eq!(value["token_usage"]["total_tokens"], 123);
        } else {
            assert_eq!(
                String::from_utf8(output).unwrap(),
                "Reused completed task retained: resolved.\n"
            );
        }
    }
    assert_eq!(
        reused[&1], summary,
        "rendering must not rewrite persisted outcome"
    );
}

#[test]
fn selected_admission_rejects_competing_membership_or_outer_identity_before_side_effects() {
    let root = Fixture::new();
    let path = root.config();
    let mut config: BenchmarkConfig =
        toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    config.concurrency = 4;
    let admission = batch::Admission::new(
        4,
        vec![4, 2],
        None,
        crate::planning::BatchAdmissionPolicy::current(),
    )
    .unwrap();
    let reused =
        std::collections::BTreeMap::from([(1, serde_json::json!({})), (3, serde_json::json!({}))]);
    for case in [
        "missing-reuse",
        "wrong-reuse",
        "outer-replica",
        "cohort",
        "material",
        "launch",
    ] {
        let wrong = std::collections::BTreeMap::from([
            (1, serde_json::json!({})),
            (2, serde_json::json!({})),
        ]);
        let mut options = RunOptions {
            reused: Some(&reused),
            ..Default::default()
        };
        match case {
            "missing-reuse" => options.reused = None,
            "wrong-reuse" => options.reused = Some(&wrong),
            "outer-replica" => options.replica = replica::Identity { index: 2, total: 4 },
            "cohort" => options.execution_cohort = Some(admission.cohort()),
            "material" => options.preparation_gate = Some(admission.material()),
            "launch" => options.launch_gate = Some(admission.launch()),
            _ => unreachable!(),
        }
        let error = run_admitted_batch(
            &path,
            config.clone(),
            options,
            &admission,
            &mut Vec::new(),
            &Mutex::new(BTreeSet::new()),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("batch admission differs"),
            "{case}: {error}"
        );
        assert_eq!(std::fs::read_dir(&root.0).unwrap().count(), 1);
    }
    // The legacy/public path is not permission to combine reuse with new
    // verified preparation just because its runtime set would be valid.
    let error = run_loaded(
        &path,
        config,
        RunOptions {
            reused: Some(&reused),
            verify_initial_material: true,
            ..Default::default()
        },
        &mut Vec::new(),
        &Mutex::new(BTreeSet::new()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("fresh preparation cannot reuse"));
}

#[cfg(unix)]
fn selected_dispatch_diagnostics(root: &Path) -> String {
    use std::io::Read;
    let mut rows = Vec::new();
    for index in [2, 4] {
        let path = root.join(format!("run-concurrency-{index}.log"));
        let mut bytes = Vec::new();
        let record = std::fs::File::open(path)
            .ok()
            .and_then(|file| file.take(65_536).read_to_end(&mut bytes).ok())
            .and_then(|_| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        rows.push(format!(
            "replica {index}: arrived={}, {}",
            root.join(format!("arrived-{index}")).is_file(),
            selected_dispatch_log_summary(record.as_ref())
        ));
    }
    rows.join("; ")
}

#[cfg(unix)]
fn selected_dispatch_log_summary(record: Option<&serde_json::Value>) -> String {
    let Some(record) = record else {
        return "log unavailable or exceeds diagnostic bound".into();
    };
    let stderr = record.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
    let markers = [
        ("peer-timeout", "real pending peer did not launch"),
        ("fetch-failed", "fetch failed"),
        ("replay-request", "fixture replay request"),
        ("connection-refused", "ECONNREFUSED"),
        ("gate", "gate"),
        ("run-error", "ASB run error:"),
        ("capture-error", "capture failed"),
    ]
    .into_iter()
    .filter_map(|(label, marker)| stderr.contains(marker).then_some(label))
    .collect::<Vec<_>>();
    let termination = record
        .get("process_termination")
        .and_then(|v| v.as_object());
    let kind = termination
        .and_then(|v| v.get("kind"))
        .and_then(|v| v.as_str())
        .filter(|v| matches!(*v, "code" | "signal"))
        .unwrap_or("unavailable");
    let value = termination
        .and_then(|v| v.get(if kind == "signal" { "signal" } else { "code" }))
        .and_then(|v| v.as_i64());
    format!(
        "termination={kind}:{value:?}, malformed={}, elapsed_ms={:?}, stdout_bytes={}, stderr_bytes={}, markers={markers:?}",
        record
            .get("process_termination_diagnostic")
            .and_then(|v| v.as_str())
            == Some("malformed"),
        record.get("elapsed_ms").and_then(|v| v.as_u64()),
        record
            .get("stdout")
            .and_then(|v| v.as_str())
            .map_or(0, str::len),
        stderr.len(),
    )
}

#[cfg(unix)]
#[test]
fn selected_dispatch_diagnostics_are_bounded_and_do_not_emit_contents() {
    let value = serde_json::json!({
        "stderr": "private-secret real pending peer did not launch /private/path",
        "stdout": "private-output", "elapsed_ms": 5000,
        "process_termination": {"kind":"code", "code":9}
    });
    let summary = selected_dispatch_log_summary(Some(&value));
    assert!(summary.contains("peer-timeout"));
    assert!(summary.contains("code:Some(9)"));
    assert!(!summary.contains("private"));
    assert!(selected_dispatch_log_summary(None).contains("unavailable"));
}

#[cfg(unix)]
#[test]
fn actual_selected_dispatch_prepares_and_launches_only_noncontiguous_original_members() {
    use crate::test_support::IsolatedEnvironment;
    use std::os::unix::fs::PermissionsExt;
    if std::env::var_os("ASB_SELECTED_DISPATCH_CASE").is_none() {
        let executable = std::env::current_exe().unwrap();
        let runtime = executable.parent().unwrap().parent().unwrap().join("asb");
        let result=Command::new(&executable).isolated_env()
            .env("PATH",std::env::var_os("PATH").unwrap_or_default())
            .args(["--exact","runner::boundary_tests::actual_selected_dispatch_prepares_and_launches_only_noncontiguous_original_members","--nocapture"])
            .env("ASB_RUNTIME_EXECUTABLE",runtime).env("ASB_SELECTED_DISPATCH_CASE","1").output().unwrap();
        assert!(
            result.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/selected-dispatch-fixtures")
        .join(workloads::TaskIdentity::local("selected", None).attempt_id);
    std::fs::create_dir_all(&root).unwrap();
    let fixture = Fixture(root.canonicalize().unwrap());
    let root = &fixture.0;
    std::fs::create_dir(root.join(".asb")).unwrap();
    let project = project::ProjectConfig {
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
    workloads::populate(&root.join("workloads")).unwrap();
    let agent = root.join("tools/pi/bin/pi");
    std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
    let node = crate::tools::host_executable(Path::new("node")).unwrap();
    let script=r#"#!NODE_PLACEHOLDER
const fs=require('node:fs');
const root=ROOT_PLACEHOLDER;
const replica=Number(process.cwd().match(/concurrency-(\d+)$/)[1]);
fs.appendFileSync(root+'/calls',replica+'\n');
fs.writeFileSync(root+'/arrived-'+replica,'ready');
(async()=>{
 const deadline=Date.now()+5000;
 while(!fs.existsSync(root+'/arrived-2')||!fs.existsSync(root+'/arrived-4')) {
  if(Date.now()>=deadline)throw new Error('real pending peer did not launch');
  await new Promise(resolve=>setTimeout(resolve,5));
 }
 const body={model:'fixture',tools:[{type:'function',function:{name:'fixture',parameters:{type:'object'}}}],messages:[]};
 const response=await fetch(process.env.OPENAI_BASE_URL+'/chat/completions',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});
 if(!response.ok)throw new Error('fixture replay request');
 await response.text();
 fs.writeFileSync(process.env.ASB_PI_SUMMARY_PATH,JSON.stringify({schema_version:1,completed:true,steps:1,usage_complete:false,final_answer:'src/cli.py src/registry.py lookup'}));
 console.log(JSON.stringify({type:'agent_end',messages:[{role:'assistant',content:[{type:'text',text:'src/cli.py src/registry.py lookup'}]}]}));
})().catch(error=>{console.error(error.message);process.exitCode=9;});
"#.replace("NODE_PLACEHOLDER",node.to_str().unwrap()).replace("ROOT_PLACEHOLDER",&serde_json::to_string(root).unwrap());
    std::fs::write(&agent, script).unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = root.join("benchmark.toml");
    std::fs::write(&path,"agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='repository-navigation'\nconcurrency=4\nworkspace='workspace'\nlog_file='run.log'\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=[]\n").unwrap();
    let config: BenchmarkConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let body = serde_json::json!({"model":"fixture","tools":[{"type":"function","function":{"name":"fixture","parameters":{"type":"object"}}}],"messages":[]});
    let cassette = root.join("input.jsonl");
    let trace = serde_json::json!({"request":{"method":"POST","url":"/v1/chat/completions","body":body.to_string()},"response":{"status":200,"headers":{"content-type":"application/json"},"body":"{}"}});
    std::fs::write(&cassette, format!("{trace}\n")).unwrap();
    let reused=std::collections::BTreeMap::from([1,3].map(|index| (index,serde_json::json!({"task":{"task_id":format!("retained-{index}")},"replica_index":index,"process_success":true,"workload_success":index!=1}))));
    let retained = reused.clone();
    for index in [1, 3] {
        let workspace = root.join(format!("workspace-concurrency-{index}"));
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("retained"), "untouched old workspace").unwrap();
        std::fs::write(
            root.join(format!("run-concurrency-{index}.log")),
            "untouched old log",
        )
        .unwrap();
    }
    let admission = batch::Admission::for_test(4, vec![2, 4], Duration::from_secs(5)).unwrap();
    let mut output = Vec::new();
    let result = run_admitted_batch(
        &path,
        config.clone(),
        RunOptions {
            reused: Some(&reused),
            json: true,
            mode: Some(tape::Mode::Replay),
            cassette: cassette.to_str(),
            ..Default::default()
        },
        &admission,
        &mut output,
        &Mutex::new(BTreeSet::new()),
    );
    assert!(
        result
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("1 concurrent benchmark replica(s) failed"),
        "selected-dispatch failure; diagnostics={}",
        selected_dispatch_diagnostics(root)
    );
    let mut calls: Vec<_> = std::fs::read_to_string(root.join("calls"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    calls.sort();
    assert_eq!(calls, vec!["2", "4"]);
    let rows: Vec<serde_json::Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 4);
    for (offset, row) in rows.iter().enumerate() {
        let index = offset + 1;
        assert_eq!(row["replica_index"], index);
        if [1, 3].contains(&index) {
            assert_eq!(row["reused"], true);
            assert_eq!(
                std::fs::read_dir(root.join(format!("workspace-concurrency-{index}")))
                    .unwrap()
                    .count(),
                1
            );
            assert_eq!(
                std::fs::read(root.join(format!("run-concurrency-{index}.log"))).unwrap(),
                b"untouched old log"
            );
            assert!(
                !root
                    .join(format!("measurements-concurrency-{index}"))
                    .exists()
            );
        } else {
            assert_eq!(row["workload_success"], true);
            assert_eq!(
                row["execution_cohort"],
                serde_json::to_value(admission.cohort()).unwrap()
            );
            assert_eq!(
                row["execution_cohort"]["dispatch"],
                serde_json::json!({"schema_version":1,"configured":4,"admitted":2})
            );
        }
    }
    assert_eq!(reused, retained);
    assert!(admission.material().confirmed_identity().is_ok());
    assert!(admission.launch().confirmed_identity().is_ok());
    let mut retry_output = Vec::new();
    let retry = run_admitted_batch(
        &path,
        config,
        RunOptions {
            reused: Some(&reused),
            ..Default::default()
        },
        &admission,
        &mut retry_output,
        &Mutex::new(BTreeSet::new()),
    )
    .unwrap_err();
    assert!(retry.to_string().contains("already consumed"));
    assert!(retry_output.is_empty());
    let mut after: Vec<_> = std::fs::read_to_string(root.join("calls"))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    after.sort();
    assert_eq!(after, calls);
}

#[cfg(unix)]
#[test]
fn gate_target_checks_absolute_interpreters_without_launching_anything() {
    let root = Fixture::new();
    assert!(!gate_target_available(
        root.0.join("missing").to_str().unwrap()
    ));
    assert!(gate_target_available("fixture-name-on-path"));
    let path = root.0.join("program");
    for (bytes, available) in [
        (b"ELF-fixture".as_slice(), true),
        (b"#!/missing/asb-fixture-interpreter\n".as_slice(), false),
        (b"#!/bin/sh\n".as_slice(), true),
        (b"#!relative-interpreter\n".as_slice(), true),
    ] {
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(gate_target_available(path.to_str().unwrap()), available);
    }
    assert_eq!(component("model:a/b Ω"), "model-a-b--");
}

#[cfg(unix)]
fn child(script: &str) -> OwnedChild {
    OwnedChild::spawn_direct(
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .unwrap()
}

#[cfg(unix)]
#[test]
fn timeout_stops_only_owned_child_and_reports_budget_once() {
    // exec replaces the shell: there are no untracked descendants or sleep races.
    let child = child("exec /bin/sleep 10");
    let pid = child.id();
    let mut callbacks = 0;
    let result = wait_with_output_streams(
        child,
        false,
        &mut Vec::new(),
        None,
        Duration::from_millis(10),
        &mut || {
            callbacks += 1;
            Ok(())
        },
    )
    .unwrap();
    assert!(!result.status.success());
    assert_eq!(callbacks, 1);
    assert_eq!(
        String::from_utf8(result.stderr).unwrap(),
        "agent execution exceeded 0.01 seconds\n"
    );
    #[cfg(target_os = "linux")]
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "owned child must be reaped"
    );
}

#[cfg(unix)]
#[test]
fn both_streams_are_forwarded_and_captured_without_losing_exit_status() {
    let mut combined = Vec::new();
    let result = wait_with_output_streams(
        child("printf out; printf err >&2; exit 7"),
        true,
        &mut combined,
        None,
        Duration::from_secs(2),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(result.status.code(), Some(7));
    assert_eq!(result.stdout, b"out");
    assert_eq!(result.stderr, b"err");
    let text = String::from_utf8(combined).unwrap();
    assert!(text == "outerr" || text == "errout", "{text}");
}

#[test]
fn unknown_agent_command_retains_generic_contract_without_adapter_guessing() {
    let config:BenchmarkConfig=toml::from_str("agent='fixture-unique-executable'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\n").unwrap();
    let command = command_for_with_interface(&config, None, "prompt", None, None).unwrap();
    assert_eq!(command.program, config.agent);
    assert!(command.args.is_empty());
    assert_eq!(
        command.prompt_delivery,
        crate::agents::PromptDelivery::Argument
    );
}

#[test]
fn max_steps_is_applied_only_by_an_explicit_adapter_capability() {
    let goose: BenchmarkConfig =
        toml::from_str("agent='goose'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\n")
            .unwrap();
    let command = command_for_with_interface(&goose, None, "prompt", None, Some(9)).unwrap();
    assert_eq!(
        command
            .args
            .windows(2)
            .find(|pair| pair[0] == "--max-turns")
            .map(|pair| pair[1].as_str()),
        Some("9")
    );

    let codex: BenchmarkConfig =
        toml::from_str("agent='codex'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\n")
            .unwrap();
    let error = match command_for_with_interface(&codex, None, "prompt", None, Some(9)) {
        Ok(_) => panic!("unsupported adapters must fail closed"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("does not support --max-steps"));
}
