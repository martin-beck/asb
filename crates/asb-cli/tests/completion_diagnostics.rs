//! Real CLI metadata-publication failures with a local, non-networked client.
#![cfg(unix)]
#[path = "support/fixture_namespace.rs"]
mod fixture_namespace;
#[cfg(unix)]
#[path = "support/fixture_writer.rs"]
mod fixture_writer;
mod support;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use support::IsolatedEnvironment;

struct Project(PathBuf);
impl Project {
    fn new(grade: bool, process_failure: bool, concurrency: usize, fault: &str) -> Self {
        let parent = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/completion-diagnostic-fixtures");
        fs::create_dir_all(&parent).unwrap();
        let root = fixture_namespace::allocate(&parent, "").unwrap();
        for directory in [".asb", "tools", "workloads", "commands"] {
            fs::create_dir_all(root.join(directory)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        fs::write(
            root.join(".asb/project.toml"),
            "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
        )
        .unwrap();
        fs::write(
            root.join("workloads/bug-fix.txt"),
            "Repair the local parser fixture.",
        )
        .unwrap();
        if fault != "input-initial" {
            symlink("/usr/bin/python3", root.join("commands/python3")).unwrap();
        }
        // No real curl exists in this private PATH; no provider is contacted.
        let curl = root.join("commands/curl");
        fixture_writer::write_file(
            &root,
            &curl,
            r#"#!/usr/bin/python3
import sys
sys.stdin.buffer.read()
sys.stdout.write('{"data":[{"id":"fixture","supported_parameters":["tools"]}]}')
sys.stderr.write('\nASB_HTTP_STATUS=200\nASB_PROXY_STATUS=0\nASB_CONTENT_TYPE=application/json\n')
"#,
        )
        .unwrap();
        fs::set_permissions(curl, fs::Permissions::from_mode(0o700)).unwrap();
        let agent = root.join("fixture-client");
        fixture_writer::write_file(&root, &agent,format!(r#"#!/usr/bin/python3
from pathlib import Path
import os,sys
root=Path(os.environ['HOME'])
with (root/'launches').open('a') as output: output.write(str(Path.cwd())+'\n')
if {grade}:
    Path('src/parser.py').write_text("def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n")
fault={fault:?}
if fault=='parallel-failure':
    import time
    (root/('barrier-'+Path.cwd().parent.name)).write_text('admitted')
    deadline=time.monotonic()+5
    while len(list(root.glob('barrier-*')))<2:
        if time.monotonic()>deadline: sys.exit(91)
        time.sleep(0.005)
    sys.exit(7 if Path.cwd().parent.name.endswith('benchmark-a') else 9)
checkpoint=root/'sweep-results/.checkpoint'
if fault=='continuation':
    (Path.cwd().parent/'.asb-completions').write_text('fixture blocks metadata')
if fault=='replica': (checkpoint/'replicas').write_text('fixture blocks receipt publication')
if fault=='group': (checkpoint/'completed').write_text('fixture blocks group publication')
if fault=='input-final':
    (checkpoint/'inputs.json').unlink()
    (checkpoint/'inputs.json').mkdir()
sys.exit({exit})
"#,grade=if grade {"True"}else{"False"},exit=if process_failure {7}else{0})).unwrap();
        fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(root.join("tools/pi/bin")).unwrap();
        symlink(&agent, root.join("tools/pi/bin/pi")).unwrap();
        fs::write(root.join("benchmark.toml"),format!("agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='bug-fix'\nconcurrency={concurrency}\nworkspace='workspace'\nlog_file='logs/run.jsonl'\n")).unwrap();
        fs::write(root.join("sweep.toml"), "configs=['benchmark.toml']\n").unwrap();
        Self(root)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_asb"))
            .args(args)
            .current_dir(&self.0)
            .isolated_env()
            .env("HOME", &self.0)
            .env("PATH", self.0.join("commands"))
            .env("OPENROUTER_API_KEY", "dummy-fixture-only")
            .output()
            .unwrap()
    }
    fn launches(&self) -> usize {
        fs::read_to_string(self.0.join("launches"))
            .unwrap()
            .lines()
            .count()
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = fixture_writer::cleanup(&self.0);
    }
}
fn json_lines(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("non-JSON stdout {line:?}: {error}"))
        })
        .collect()
}

#[test]
fn blocked_task_record_preserves_each_grade_primary_failure_and_concurrent_json_summary() {
    for grade in [true, false] {
        for process_failure in [true, false] {
            for json in [true, false] {
                let project = Project::new(grade, process_failure, 4, "continuation");
                let args = if json {
                    vec!["run", "benchmark.toml", "--json"]
                } else {
                    vec!["run", "benchmark.toml"]
                };
                let output = project.run(&args);
                assert_eq!(
                    output.status.success(),
                    grade && !process_failure,
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    project.launches(),
                    4,
                    "metadata must never retry the client"
                );
                if json {
                    let lines = json_lines(&output);
                    let warnings = lines
                        .iter()
                        .filter(|line| line["event"] == "continuation_warning")
                        .collect::<Vec<_>>();
                    assert_eq!(warnings.len(), 1, "{lines:?}");
                    assert_eq!(warnings[0]["code"], "task_outcome_unavailable");
                    let summaries = lines
                        .iter()
                        .filter(|line| line.get("event").is_none())
                        .collect::<Vec<_>>();
                    assert_eq!(summaries.len(), 4);
                    for summary in summaries {
                        assert_eq!(summary["workload_success"], grade);
                        assert_eq!(summary["process_success"], !process_failure);
                        if process_failure {
                            assert!(summary["failure_reason"].as_str().unwrap().contains("7"));
                        }
                    }
                } else {
                    let text = String::from_utf8_lossy(&output.stdout);
                    assert_eq!(
                        text.matches("private task outcome unavailable").count(),
                        1,
                        "{text}"
                    );
                    assert!(text.contains(".asb-completions"));
                    assert!(text.contains("Do not rerun") || text.contains("do not rerun"));
                }
                for index in 1..=4 {
                    let path = project.0.join(format!(
                        "results/benchmark-runs/run-001/run-concurrency-{index}.log"
                    ));
                    let log: Value = serde_json::from_str(
                        fs::read_to_string(path).unwrap().lines().last().unwrap(),
                    )
                    .unwrap();
                    assert_eq!(log["workload_success"], grade);
                    assert_eq!(log["process_success"], !process_failure);
                }
            }
        }
    }
}

#[test]
fn actual_replica_group_and_final_input_publication_faults_warn_without_retry_or_grade_changes() {
    for fault in ["replica", "group", "input-final", "input-initial"] {
        for grade in [true, false] {
            let project = Project::new(grade, false, 1, fault);
            let output = project.run(&["sweep", "sweep.toml", "--json"]);
            let lines = json_lines(&output);
            let summaries = lines
                .iter()
                .filter(|value| value.get("task").is_some())
                .collect::<Vec<_>>();
            assert_eq!(
                summaries.len(),
                1,
                "{fault}: {lines:?}; {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(summaries[0]["workload_success"], grade);
            assert_eq!(
                summaries[0]["process_success"],
                true,
                "{fault}: {lines:?}; {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                output.status.success(),
                grade && fault != "replica",
                "{fault}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if fault == "replica" {
                // The pre-existing report reader conservatively rejects this
                // deliberately non-directory provenance path. That separate
                // report error must not erase the already-published grade.
                assert!(String::from_utf8_lossy(&output.stderr).contains("receipt provenance"));
            }
            let expected = match fault {
                "replica" => "replica_receipt_unavailable",
                "group" => "group_receipt_unavailable",
                "input-final" => "input_seal_unavailable",
                _ => "initial_input_seal_unavailable",
            };
            assert!(lines.iter().any(|line|line["event"]=="continuation_warning"&&line["code"]==expected),"{fault}: {lines:?}");
            if fault == "replica" {
                let warning = lines.iter().find(|line| line["code"] == expected).unwrap();
                assert_eq!(warning["path_role"], "destination");
                assert!(
                    warning["path"]
                        .as_str()
                        .unwrap()
                        .contains(".checkpoint/replicas/")
                );
                assert!(
                    !warning["path"]
                        .as_str()
                        .unwrap()
                        .contains(".asb-completions")
                );
            }
            assert_eq!(project.launches(), 1);
            let resumed = project.run(&["sweep", "sweep.toml", "--resume", "--json"]);
            if fault == "replica" || fault == "input-final" || fault == "input-initial" {
                assert!(
                    !resumed.status.success(),
                    "missing required evidence must not authorize reuse"
                );
            }
            assert_eq!(
                project.launches(),
                1,
                "resume must never silently repeat an existing task"
            );
        }
    }
}

#[test]
fn successful_task_record_keeps_the_normal_json_output_shape() {
    let project = Project::new(true, false, 1, "");
    let output = project.run(&["run", "benchmark.toml", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = json_lines(&output);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["workload_success"], true);
    assert!(lines[0].get("event").is_none());
    assert_eq!(project.launches(), 1);
}

#[test]
fn isolated_real_warning_writer_failure_preserves_grade_and_the_primary_error() {
    use std::ffi::OsString;
    use std::io::Write;
    const MARKER: &str = "ASB_CONTINUATION_BROKEN_WRITER";
    if std::env::var_os(MARKER).is_none() {
        for (command, fault) in [("run", ""), ("sweep", "group"), ("sweep", "input-final")] {
            for grade in [true, false] {
                for process_failure in [true, false] {
                    let project = Project::new(
                        grade,
                        process_failure,
                        1,
                        if command == "run" {
                            "continuation"
                        } else {
                            fault
                        },
                    );
                    let output=Command::new(std::env::current_exe().unwrap())
                .args(["--exact","isolated_real_warning_writer_failure_preserves_grade_and_the_primary_error","--nocapture"])
                .current_dir(&project.0).isolated_env().env(MARKER,"1")
                .env("ASB_CONTINUATION_WRITER_COMMAND",command)
                .env("HOME",&project.0).env("PATH",project.0.join("commands"))
                .env("OPENROUTER_API_KEY","dummy-fixture-only")
                .env("ASB_RUNTIME_EXECUTABLE",env!("CARGO_BIN_EXE_asb"))
                .output().unwrap();
                    assert!(
                        output.status.success(),
                        "{}\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let summary: Value = serde_json::from_slice(
                        &fs::read(project.0.join("writer-summary.json")).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(summary["workload_success"], grade);
                    assert_eq!(summary["process_success"], !process_failure);
                    let errors = fs::read_to_string(project.0.join("writer-error.txt")).unwrap();
                    assert!(
                        errors.contains("fixture diagnostic writer closed"),
                        "{errors}"
                    );
                    if !grade || process_failure {
                        assert!(
                            errors.contains(summary["failure_reason"].as_str().unwrap()),
                            "primary error lost: {errors}"
                        );
                    }
                    assert_eq!(project.launches(), 1);
                }
            }
        }
        return;
    }
    struct SummaryOnly {
        bytes: Vec<u8>,
        complete: bool,
    }
    impl Write for SummaryOnly {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.complete {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "fixture diagnostic writer closed",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            self.complete = bytes.contains(&b'\n');
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = SummaryOnly {
        bytes: Vec::new(),
        complete: false,
    };
    let mut errors = Vec::new();
    let command = std::env::var("ASB_CONTINUATION_WRITER_COMMAND").unwrap();
    let config = if command == "run" {
        "benchmark.toml"
    } else {
        "sweep.toml"
    };
    let code = asb_cli::run(
        &[
            OsString::from(command),
            OsString::from(config),
            OsString::from("--json"),
        ],
        &mut output,
        &mut errors,
    );
    assert_ne!(code, 0, "an output failure must not be silently ignored");
    let summary: Value = serde_json::from_slice(&output.bytes).unwrap();
    fs::write("writer-summary.json", serde_json::to_vec(&summary).unwrap()).unwrap();
    fs::write("writer-error.txt", errors).unwrap();
}

#[test]
fn isolated_parallel_sweep_retains_both_admitted_failures_when_output_breaks() {
    use std::ffi::OsString;
    use std::io::Write;
    const MARKER: &str = "ASB_PARALLEL_SWEEP_BROKEN_WRITER";
    if std::env::var_os(MARKER).is_none() {
        let project = Project::new(true, true, 1, "parallel-failure");
        let config = fs::read_to_string(project.0.join("benchmark.toml")).unwrap();
        for suffix in ["a", "b"] {
            fs::write(
                project.0.join(format!("benchmark-{suffix}.toml")),
                config
                    .replace(
                        "workspace='workspace'",
                        &format!("workspace='workspace-{suffix}'"),
                    )
                    .replace(
                        "log_file='logs/run.jsonl'",
                        &format!("log_file='logs/run-{suffix}.jsonl'"),
                    ),
            )
            .unwrap();
        }
        fs::write(
            project.0.join("sweep.toml"),
            "configs=['benchmark-a.toml','benchmark-b.toml']\n",
        )
        .unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "isolated_parallel_sweep_retains_both_admitted_failures_when_output_breaks",
                "--nocapture",
            ])
            .current_dir(&project.0)
            .isolated_env()
            .env(MARKER, "1")
            .env("HOME", &project.0)
            .env("PATH", project.0.join("commands"))
            .env("OPENROUTER_API_KEY", "dummy-fixture-only")
            .env("ASB_RUNTIME_EXECUTABLE", env!("CARGO_BIN_EXE_asb"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let errors = fs::read_to_string(project.0.join("parallel-writer-error.txt")).unwrap();
        assert!(
            errors.contains("fixture parallel writer closed"),
            "{errors}"
        );
        for (index, suffix) in ["a", "b"].iter().enumerate() {
            let log = fs::read_to_string(project.0.join(format!(
                "sweep-results/{index:03}-benchmark-{suffix}/run.log"
            )))
            .unwrap();
            let summary: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
            assert_eq!(summary["workload_success"], true);
            assert_eq!(summary["process_success"], false);
            assert!(
                errors.contains(summary["failure_reason"].as_str().unwrap()),
                "primary {suffix} lost: {errors}"
            );
            assert!(
                errors.contains(&format!("benchmark-{suffix}.toml")),
                "{errors}"
            );
        }
        assert_eq!(
            project.launches(),
            2,
            "no retry or unfinished admitted execution"
        );
        return;
    }
    struct Broken {
        writes: usize,
    }
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "fixture parallel writer closed",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Broken { writes: 0 };
    let mut errors = Vec::new();
    let args = ["sweep", "sweep.toml", "--jobs", "2", "--json"].map(OsString::from);
    assert_ne!(asb_cli::run(&args, &mut output, &mut errors), 0);
    assert_eq!(
        output.writes, 1,
        "do not attempt further terminal output after failure"
    );
    fs::write("parallel-writer-error.txt", errors).unwrap();
}
