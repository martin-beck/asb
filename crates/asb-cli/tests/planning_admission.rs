//! Real CLI rejection before providers, collectors, environments or output moves.
#[path = "support/fixture_namespace.rs"]
mod fixture_namespace;
#[cfg(unix)]
#[path = "support/fixture_writer.rs"]
mod fixture_writer;
mod support;
use std::{path::PathBuf, process::Command};
use support::IsolatedEnvironment;

struct Project(PathBuf);
impl Project {
    fn new() -> Self {
        let parent =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/planning-fixtures");
        std::fs::create_dir_all(&parent).unwrap();
        let root = fixture_namespace::allocate(&parent, "").unwrap();
        std::fs::create_dir_all(root.join(".asb")).unwrap();
        std::fs::write(
            root.join(".asb/project.toml"),
            "tools_dir='.asb-tools'\nworkloads_dir='workloads'\nworkspace_dir='asb-workspace'\n",
        )
        .unwrap();
        Self(root)
    }
    fn write(&self, text: &str) {
        std::fs::write(
            self.0.join("benchmark.toml"),
            format!(
                "agent='pi'\nworkload='bug-fix'\nprovider='openrouter'\nmodel='fixture'\n{text}"
            ),
        )
        .unwrap();
    }
    fn reject(&self, args: &[&str], reason: &str) {
        self.reject_with_environment(args, reason, &[]);
    }
    fn reject_with_environment(&self, args: &[&str], reason: &str, environment: &[(&str, &str)]) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_asb"));
        let output = command
            .isolated_env()
            .env("HOME", &self.0)
            .envs(environment.iter().copied())
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success(), "unexpected success");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(reason), "expected {reason:?}: {error}");
        for path in [
            "asb-workspace",
            ".asb-tools",
            ".asb/attempts",
            ".asb/execution-plans",
            "measurements",
            "benchmark.log",
        ] {
            assert!(!self.0.join(path).exists(), "preflight created {path}");
        }
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = fixture_writer::cleanup(&self.0);
        #[cfg(not(unix))]
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
#[test]
fn native_plan_executes_with_the_validated_budget_and_keeps_output_for_grading() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new();
    let script = project.0.join("slow-native-fixture.sh");
    fixture_writer::write_file(
        &project.0,
        &script,
        "#!/bin/sh\nprintf 'native-fixture-started\\n'\nexec sleep 5\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir_all(project.0.join("workloads")).unwrap();
    std::fs::write(
        project.0.join("workloads/bug-fix.txt"),
        "Repair the parser in the provided workspace.",
    )
    .unwrap();
    std::fs::write(project.0.join("benchmark.toml"), format!("agent='{}'\nprovider='cli2key'\nmodel='test-model'\nworkload='bug-fix'\nlog_file='native.log'\n[planning.budgets]\nexecution_seconds=1\n", script.display())).unwrap();
    let cassette = serde_json::json!({"request":{"method":"POST","url":"/v1/responses","body":"{\"model\":\"test-model\",\"tools\":[{\"type\":\"function\"}]}"},"response":{"status":200,"headers":{"content-type":"application/json"},"body":"{}"}});
    std::fs::write(project.0.join("fixture.jsonl"), format!("{cassette}\n")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_asb"))
        .isolated_env()
        .current_dir(&project.0)
        .env("HOME", &project.0)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args([
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "fixture.jsonl",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let result_root = project.0.join("results/benchmark-runs");
    let run_directory = std::fs::read_dir(&result_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .expect("direct run result directory");
    let text = std::fs::read_to_string(run_directory.join("run.log"))
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    let record: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
    assert!(
        record["stdout"]
            .as_str()
            .unwrap()
            .contains("native-fixture-started")
    );
    assert!(
        record["stderr"]
            .as_str()
            .unwrap()
            .contains("agent execution exceeded 1 seconds")
    );
    assert!(
        record["elapsed_ms"].as_u64().unwrap() < 4000,
        "a default-budget execution would have waited five seconds"
    );
    assert_eq!(record["effective_budgets"]["execution_seconds"], 1.0);
    assert_eq!(record["planning"]["budgets"]["execution_seconds"], 1);
    assert!(
        record["workload_success"].is_boolean(),
        "a timed-out native client must still reach grading"
    );
    assert!(
        record["execution_plan_fingerprint"]
            .as_str()
            .unwrap()
            .starts_with("asb-protocol-v1:")
    );
}

#[test]
fn backend_and_full_preparation_deadlines_fail_before_side_effects() {
    let project = Project::new();
    for (config, reason) in [
        ("[planning]\nbackend='remote'", "backend is unsupported"),
        (
            "[planning]\nrequired_policies=['network-denied']",
            "cannot be enforced",
        ),
        ("[planning.budgets]\npreparation_seconds=1", "unknown field"),
        (
            "[planning.budgets]\nagent_preparation_seconds=1",
            "end-to-end deadline",
        ),
        (
            "[planning.repetition]\nindex=0\ntotal=2",
            "repetition index",
        ),
    ] {
        project.write(config);
        project.reject(&["run", "benchmark.toml"], reason);
    }
}

#[test]
fn measurement_override_negotiates_collectors_even_when_configuration_disables_collection() {
    let project = Project::new();
    project.write(
        "[measurements]\nenabled=false\ndirectory='measurements'\ncollectors=['unknown-collector']",
    );
    project.reject(
        &["run", "benchmark.toml", "--measurements", "measurements"],
        "unknown measurement collector",
    );
    let config = "agent='pi'\nworkload='terminal-bench-2.1'\nprovider='openrouter'\nmodel='fixture'\n[planning]\nrequired_policies=['verified-local-target']\n";
    std::fs::write(project.0.join("benchmark.toml"), config).unwrap();
    project.reject(&["run", "benchmark.toml"], "cannot be enforced");
    for variable in [
        ("DOCKER_HOST", "ssh://remote"),
        ("DOCKER_CONTEXT", "remote"),
    ] {
        project.reject_with_environment(
            &["run", "benchmark.toml"],
            "remote Docker endpoints/contexts",
            &[variable],
        );
    }
}

#[test]
fn docker_persisted_context_is_checked_before_any_startup_without_a_docker_program() {
    let project = Project::new();
    let docker = project.0.join(".docker");
    std::fs::create_dir_all(&docker).unwrap();
    std::fs::write(project.0.join("benchmark.toml"), "agent='pi'\nworkload='terminal-bench-2.1'\nprovider='openrouter'\nmodel='fixture'\n[planning]\nrequired_policies=['verified-local-target']\n").unwrap();
    for content in [
        r#"{"currentContext":"remote-prod"}"#,
        r#"{"currentContext":"unknown-context"}"#,
    ] {
        std::fs::write(docker.join("config.json"), content).unwrap();
        project.reject(&["run", "benchmark.toml"], "persisted currentContext");
        // Conservative rejection avoids CLI/SDK precedence disagreements.
        project.reject_with_environment(
            &["run", "benchmark.toml"],
            "persisted currentContext",
            &[
                ("DOCKER_CONTEXT", "default"),
                ("DOCKER_HOST", "unix:///tmp/local.sock"),
            ],
        );
    }
    for content in [
        "{",
        "null",
        "[]",
        r#"{"currentContext":42}"#,
        r#"{"currentContext":"remote","currentContext":"default"}"#,
    ] {
        std::fs::write(docker.join("config.json"), content).unwrap();
        project.reject(&["run", "benchmark.toml"], "cannot validate local Docker");
    }
    std::fs::write(docker.join("config.json"), vec![b' '; 1024 * 1024 + 1]).unwrap();
    project.reject(&["run", "benchmark.toml"], "cannot validate local Docker");
    for content in [
        "{}",
        r#"{"currentContext":""}"#,
        r#"{"currentContext":"default"}"#,
    ] {
        std::fs::write(docker.join("config.json"), content).unwrap();
        // Reaching the unrelated policy check proves metadata-only validation
        // succeeds even with PATH cleared and no Docker daemon/executable.
        project.reject(&["run", "benchmark.toml"], "cannot be enforced");
    }
    let custom = project.0.join("custom-docker");
    std::fs::create_dir_all(&custom).unwrap();
    std::fs::write(custom.join("config.json"), r#"{"currentContext":"remote"}"#).unwrap();
    project.reject_with_environment(
        &["run", "benchmark.toml"],
        "persisted currentContext",
        &[("DOCKER_CONFIG", custom.to_str().unwrap())],
    );
    project.reject_with_environment(
        &["run", "benchmark.toml"],
        "cannot be enforced",
        &[(
            "DOCKER_CONFIG",
            project.0.join("absent-docker").to_str().unwrap(),
        )],
    );
}

#[cfg(unix)]
#[test]
fn docker_config_special_files_fail_closed_and_native_ignores_docker_entirely() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let project = Project::new();
    let directory = project.0.join("docker-config");
    std::fs::create_dir_all(&directory).unwrap();
    let config = directory.join("config.json");
    std::fs::write(&config, "{}").unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::write(
        project.0.join("benchmark.toml"),
        "agent='pi'\nworkload='terminal-bench-2.1'\nprovider='openrouter'\nmodel='fixture'\n",
    )
    .unwrap();
    // Privileged CI users can still read mode-000 files; assert this case only
    // when the filesystem actually makes the fixture inaccessible.
    if std::fs::File::open(&config).is_err() {
        project.reject_with_environment(
            &["run", "benchmark.toml"],
            "cannot validate local Docker",
            &[("DOCKER_CONFIG", directory.to_str().unwrap())],
        );
    }
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::remove_file(&config).unwrap();
    symlink(project.0.join("missing"), &config).unwrap();
    std::fs::write(
        project.0.join("benchmark.toml"),
        "agent='pi'\nworkload='terminal-bench-2.1'\nprovider='openrouter'\nmodel='fixture'\n",
    )
    .unwrap();
    project.reject_with_environment(
        &["run", "benchmark.toml"],
        "cannot validate local Docker",
        &[("DOCKER_CONFIG", directory.to_str().unwrap())],
    );
    std::fs::remove_file(&config).unwrap();
    // A FIFO must not hang the preflight even with no writer.
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(&config)
            .status()
            .unwrap()
            .success()
    );
    project.reject_with_environment(
        &["run", "benchmark.toml"],
        "cannot validate local Docker",
        &[("DOCKER_CONFIG", directory.to_str().unwrap())],
    );
    let fake = project.0.join("docker");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf invoked > '{}'\nexit 1\n",
            project.0.join("docker-invoked").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    project.write("[planning]\nbackend='remote'");
    project.reject_with_environment(
        &["run", "benchmark.toml"],
        "backend is unsupported",
        &[
            ("DOCKER_CONFIG", directory.to_str().unwrap()),
            ("DOCKER_HOST", "ssh://remote"),
            ("DOCKER_CONTEXT", "remote"),
            ("PATH", project.0.to_str().unwrap()),
        ],
    );
    assert!(!project.0.join("docker-invoked").exists());
}

#[test]
fn impossible_or_unknown_replica_resources_preserve_previous_results() {
    let project = Project::new();
    std::fs::create_dir_all(project.0.join("sweep-results")).unwrap();
    std::fs::write(project.0.join("sweep-results/keep"), "previous results").unwrap();
    std::fs::write(project.0.join("sweep.toml"), "configs=['benchmark.toml']\n[resources]\ncpu_millis=2000\nmemory_mib=8192\ndisk_mib=4096\n").unwrap();
    for (config, reason) in [
        (
            "concurrency=3\n[planning.reservation]\ncpu_millis=1000\nmemory_mib=4096\ndisk_mib=2048",
            "exceeds sweep resources",
        ),
        ("", "explicit planning.reservation"),
        (
            "[planning.reservation]\ncpu_millis=0\nmemory_mib=0\ndisk_mib=0",
            "empty reservations",
        ),
    ] {
        project.write(config);
        project.reject(&["sweep", "sweep.toml"], reason);
        assert_eq!(
            std::fs::read_to_string(project.0.join("sweep-results/keep")).unwrap(),
            "previous results"
        );
        assert!(!project.0.join("sweep-history").exists());
    }
}
