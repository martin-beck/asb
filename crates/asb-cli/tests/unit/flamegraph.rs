//! Synthetic post-processing only: immutable true/false tools, never host perf.
use super::*;
use std::fs;
use std::path::PathBuf;

#[cfg(unix)]
#[test]
fn isolated_flamegraph_missing_inputs_pipeline_failures_and_owned_timeout() {
    use std::os::unix::fs::symlink;
    const CASE: &str = "ASB_FLAMEGRAPH_FIXTURE_CASE";
    let Ok(case) = std::env::var(CASE) else {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/flamegraph-status-fixtures")
            .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let name = format!(
            "{}::isolated_flamegraph_missing_inputs_pipeline_failures_and_owned_timeout",
            module_path!().split_once("::").unwrap().1
        );
        for case in [
            "no-artifact",
            "missing-data",
            "empty-data",
            "missing-tools",
            "perf-failed",
            "collapse-failed",
            "empty-folded",
            "render-failed",
            "completed",
            "timeout",
        ] {
            let directory = root.join(case);
            fs::create_dir_all(directory.join("empty-path")).unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", &name, "--nocapture"])
                .current_dir(&directory)
                .env_clear()
                .env(CASE, case)
                .env("HOME", &directory)
                .env("PATH", directory.join("empty-path"));
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
    if case == "timeout" {
        // PATH contains no pkill. Only this directly owned child is stopped.
        let mut child = Command::new("/bin/sleep").arg("10").spawn().unwrap();
        assert!(!wait_bounded(&mut child, Duration::ZERO).unwrap());
        assert!(!child.try_wait().unwrap().unwrap().success());
        return;
    }
    let tools = root.join("tools");
    for (tool, name, failing) in [
        ("perf", "perf", case == "perf-failed"),
        (
            "flamegraph",
            "stackcollapse-perf.pl",
            case == "collapse-failed",
        ),
        (
            "flamegraph",
            "flamegraph",
            case == "render-failed" || case == "empty-folded",
        ),
    ] {
        let path = tools.join(tool).join("bin").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if case != "missing-tools" || tool == "perf" {
            let target = if failing {
                "/bin/false"
            } else if name == "stackcollapse-perf.pl" && case != "empty-folded" {
                // Normal pipeline cases need an actual folded frame. The
                // dedicated empty-folded case deliberately uses /bin/true.
                "/bin/echo"
            } else {
                "/bin/true"
            };
            symlink(target, path).unwrap();
        }
    }
    let mut session = measurements::Session::start_in_execution_namespace(
        &root.join("measurements"),
        &["agent-memory".into()],
    )
    .unwrap();
    if case == "missing-tools" {
        let mut sudo = std::collections::BTreeSet::new();
        let selected = vec!["flamegraph".to_owned()];
        let mut context = crate::collectors::PreflightContext {
            project_root: None,
            states: &mut crate::collectors::CollectorStates::default(),
            measurement_dir: &session.dir,
            selected: &selected,
            tool_dir: Some(&tools),
            sudo_collectors: &mut sudo,
        };
        let error = ADAPTER.preflight(&mut context).unwrap_err();
        assert!(error.contains("flamegraph scripts are required"));
        assert!(error.contains("asb tool install flamegraph"));
        assert!(
            sudo.is_empty(),
            "conversion must not request host privileges"
        );
    }
    session.tool_dir = Some(tools);
    if case != "no-artifact" {
        let data = root.join("perf.data");
        if case != "missing-data" {
            fs::write(
                &data,
                if case == "empty-data" {
                    b"".as_slice()
                } else {
                    b"synthetic sample fixture".as_slice()
                },
            )
            .unwrap();
        }
        session.set_artifact(crate::collectors::SharedArtifact::StackSamples, data);
    }
    ADAPTER.finish(&mut session).unwrap();
    let status: serde_json::Value =
        serde_json::from_slice(&fs::read(session.dir.join("flamegraph.json")).unwrap()).unwrap();
    assert_eq!(status["schema_version"], 1);
    if case == "completed" {
        assert_eq!(status["status"], "completed");
        assert_eq!(status["artifact"], "flamegraph.svg");
        assert!(status["reason"].is_null());
        assert!(session.dir.join("flamegraph.svg").is_file());
    } else {
        assert_eq!(status["status"], "unavailable");
        assert!(status["artifact"].is_null());
        let reason = status["reason"].as_str().unwrap();
        let expected = match case.as_str() {
            "no-artifact" => "recording did not start",
            "missing-data" | "empty-data" => "produced no samples",
            "missing-tools" => "install stackcollapse-perf.pl",
            "perf-failed" => "perf script timed out",
            "collapse-failed" => "stack collapse script timed out",
            "empty-folded" => "no stack samples",
            "render-failed" => "renderer timed out",
            _ => unreachable!(),
        };
        assert!(reason.contains(expected), "{reason}");
    }
}
