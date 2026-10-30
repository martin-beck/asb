//! Source/cache orchestration uses fixture SDK semantics, not qualification of
//! an upstream dataset. Runtime repair is excluded by positive fixture guards;
//! the fake verifier computes a real digest of its tiny fixture archive.
use super::*;
use crate::workloads::adapters::WorkloadAdapter;
use serde_json::json;
use std::path::PathBuf;
use std::process::Command;
const ADAPTER: TerminalBenchAdapter = TerminalBenchAdapter {
    id: "fixture-suite",
    source: "fixture/source",
    revision: "pinned",
    split: "test",
    archive: "https://must-never-contact.invalid/archive",
    engine: crate::workloads::adapters::terminal_bench::ORIGINAL_ENGINE,
    version: "fixture-sdk",
    expected_tasks: 1,
    tasks_root: "tasks",
};
#[cfg(unix)]
#[test]
fn isolated_catalog_source_download_and_cache_preservation_contracts() {
    use std::os::unix::fs::PermissionsExt;
    const CASE: &str = "ASB_TERMINAL_SOURCE_CASE";
    let Ok(case) = std::env::var(CASE) else {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/terminal-source-fixtures")
            .join(TaskIdentity::local("fixture", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let python = crate::tools::host_executable(Path::new("python3")).unwrap();
        let name = format!(
            "{}::isolated_catalog_source_download_and_cache_preservation_contracts",
            module_path!().split_once("::").unwrap().1
        );
        for case in [
            "fresh",
            "damaged-cache",
            "download-fails",
            "download-127",
            "archive-fails",
            "missing-curl",
            "cache-blocked",
            "missing-tool",
        ] {
            let directory = root.join(case);
            fs::create_dir_all(directory.join("commands")).unwrap();
            fs::create_dir_all(directory.join(".asb")).unwrap();
            fs::write(
                directory.join(".asb/project.toml"),
                toml::to_string(&crate::project::ProjectConfig {
                    tools_dir: "tools".into(),
                    ..Default::default()
                })
                .unwrap(),
            )
            .unwrap();
            if case != "missing-curl" {
                let curl = directory.join("commands/curl");
                crate::write_test_file(&directory, &curl,r#"#!/bin/sh
printf '%s\n' "$*" > "$HOME/curl-arguments"
[ "$ASB_TERMINAL_SOURCE_CASE" != download-fails ] || exit 2
[ "$ASB_TERMINAL_SOURCE_CASE" != download-127 ] || exit 127
while [ "$#" -gt 0 ]; do
 if [ "$1" = --output ]; then
  shift
  if [ "$ASB_TERMINAL_SOURCE_CASE" = archive-fails ]; then printf 'corrupt archive'; else printf 'fixture terminal archive'; fi > "$1"
 fi
 shift
done
"#).unwrap();
                fs::set_permissions(curl, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", &name, "--nocapture"])
                .current_dir(&directory)
                .env_clear()
                .env(CASE, case)
                .env("HOME", &directory)
                .env("PATH", directory.join("commands"))
                .env("ASB_TERMINAL_FIXTURE_PYTHON", &python);
            if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
                command.env("LLVM_PROFILE_FILE", profile);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        }
        fs::remove_dir_all(root).unwrap();
        return;
    };
    let root = PathBuf::from(std::env::var_os("HOME").unwrap());
    let tools = root.join("tools");
    let tool = tools.join(ADAPTER.engine.tool());
    let workloads = root.join("workloads");
    let dataset = workloads.join(ADAPTER.id);
    fs::create_dir_all(dataset.join("source/tasks/task-1")).unwrap();
    fs::write(dataset.join("source-manifest.json"), "{}").unwrap();
    let catalog = json!({"schema_version":1,"workload":ADAPTER.id,
        "dataset":{"source":ADAPTER.source,"revision":ADAPTER.revision,"split":ADAPTER.split},
        "sdk":{"package":ADAPTER.engine.tool(),"version":ADAPTER.version},"source_sha256":"a".repeat(64),"archive_sha256":"b".repeat(64),
        "tasks":[{"id":"task-1","description":"fixture task","instruction":"fixture instruction","definition":"source/tasks/task-1"}]});
    fs::write(
        dataset.join("catalog.json"),
        serde_json::to_vec(&catalog).unwrap(),
    )
    .unwrap();
    fs::create_dir_all(tool.join("venv/bin")).unwrap();
    let marker = tool.join("venv/bin/tb");
    fs::write(&marker, "fixture SDK presence only").unwrap();
    fs::set_permissions(marker, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        install::tool_installed_locally(ADAPTER.engine.tool(), &tools),
        "must not install an SDK"
    );
    let python = PathBuf::from(std::env::var_os("ASB_TERMINAL_FIXTURE_PYTHON").unwrap());
    let interpreter = format!(
        r#"#!{}
import hashlib,os,sys
from pathlib import Path
operation=sys.argv[2]
with Path(os.environ['HOME']).joinpath('sdk-operations').open('a') as log: log.write(operation+'\n')
if operation=='runtime': sys.exit(0)
if operation=='verify': sys.exit(2)
if operation in ('archive','install'):
 archive=Path(sys.argv[sys.argv.index('--archive')+1])
 assert hashlib.sha256(archive.read_bytes()).digest()==hashlib.sha256(b'fixture terminal archive').digest()
 sys.exit(0)
raise AssertionError('unrecognized fixture operation')
"#,
        python.display()
    );
    let executable = tool.join("venv/bin/python");
    fs::write(&executable, &interpreter).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let cache = root.join(".asb-cache/workload-sources");
    let archive = cache.join("fixture-suite-pinned.tar.gz");
    if case == "damaged-cache" {
        fs::create_dir_all(&cache).unwrap();
        fs::write(&archive, b"original damaged bytes").unwrap();
    }
    if case == "cache-blocked" {
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        fs::write(&cache, "not a directory").unwrap();
    }
    let context = InstallationContext {
        process: &crate::install::OperationContext::new(
            &root,
            &crate::project::ProjectConfig::default(),
        )
        .unwrap(),
        project_root: &root,
        tools_root: &tools,
        workloads_root: &workloads,
    };
    let mut output = Vec::new();
    let mut stages = Vec::new();
    let missing = TerminalBenchAdapter {
        engine: crate::workloads::adapters::official_engine::EngineBinding::new(
            "nonexistent-fixture-tool",
            "asb_terminal_bench_1",
            super::ensure_helpers,
        ),
        ..ADAPTER
    };
    if case == "missing-tool" {
        assert!(crate::tools::find(missing.engine.tool()).is_none());
    }
    let adapter = if case == "missing-tool" {
        &missing
    } else {
        &ADAPTER
    };
    let result = adapter.install(&context, &mut output, &mut |percent, _| {
        stages.push(percent)
    });
    assert_eq!(
        fs::read_to_string(&executable).unwrap(),
        interpreter,
        "SDK fixture must never be reinstalled"
    );
    if case == "fresh" || case == "damaged-cache" {
        assert!(result.unwrap());
        assert!(stages.contains(&20));
        assert!(stages.contains(&50));
        assert!(stages.contains(&100));
        assert_eq!(fs::read(&archive).unwrap(), b"fixture terminal archive");
        let args = fs::read_to_string(root.join("curl-arguments")).unwrap();
        assert!(args.contains(ADAPTER.archive));
        assert!(args.contains("--max-filesize 268435456"));
        assert!(workloads.join("fixture-suite.workload.toml").is_file());
        if case == "damaged-cache" {
            assert!(
                String::from_utf8(output)
                    .unwrap()
                    .contains("Preserved damaged source archive")
            );
            let backup = fs::read_dir(&cache)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.to_string_lossy().contains(".damaged-"))
                .unwrap();
            assert_eq!(fs::read(backup).unwrap(), b"original damaged bytes");
        }
    } else {
        let error = result.unwrap_err().to_string();
        let expected = match case.as_str() {
            "download-fails" | "download-127" => {
                "could not download official Terminal-Bench task source".to_owned()
            }
            "archive-fails" => "incomplete or unsafe".to_owned(),
            "missing-curl" => "official Terminal-Bench task source needs `curl`".to_owned(),
            "cache-blocked" => fs::create_dir_all(&cache).unwrap_err().to_string(),
            "missing-tool" => "unknown tool".to_owned(),
            _ => unreachable!(),
        };
        assert!(error.contains(&expected), "{error}");
        assert!(!stages.contains(&100));
        if matches!(
            case.as_str(),
            "download-fails" | "download-127" | "missing-curl"
        ) {
            assert_eq!(stages, [20]);
            assert!(!archive.exists());
            assert!(!workloads.join("fixture-suite.workload.toml").exists());
            assert_eq!(
                fs::read_to_string(root.join("sdk-operations")).unwrap(),
                "runtime\nverify\n"
            );
            assert_eq!(root.join("curl-arguments").exists(), case != "missing-curl");
            if case == "download-127" {
                assert!(error.contains("exit status: 127"), "{error}");
                assert!(!error.contains("needs `curl`"));
            }
        }
    }
}
