//! Provider-free setup contracts: catalogs are synthetic, no SDK is executed.
use serde_json::json;
#[path = "support/setup_installables.rs"]
mod setup_installables;
mod support;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use support::IsolatedEnvironment;

struct Project(PathBuf);
impl Project {
    fn new() -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let value = Self(std::env::temp_dir().join(format!(
            "asb-setup-selection-{}-{stamp}",
            std::process::id()
        )));
        value.write(
            ".asb/project.toml",
            "tools_dir='tools'\nworkloads_dir='catalogs'\nworkspace_dir='workspaces'\n",
        );
        setup_installables::seed(&value.0, "tools");
        value
    }
    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_asb"))
            .current_dir(&self.0)
            .isolated_env()
            .env("HOME", &self.0)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .args(args)
            .output()
            .unwrap()
    }
    /// Selection coverage must not make an external agent installation part of
    /// its synthetic-catalog contract.  The automatic installer itself has
    /// focused coverage elsewhere.
    fn seed_pi(&self) {
        self.write_executable("tools/pi/bin/pi");
    }
    fn write_executable(&self, relative: &str) {
        let executable = self.0.join(relative);
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn seed_external_sdks(&self) {
        for (tool, executable) in [
            ("swebench", "swebench"),
            ("terminal-bench", "tb"),
            ("harbor", "harbor"),
        ] {
            self.write_executable(&format!("tools/{tool}/venv/bin/{executable}"));
            self.write_executable(&format!("tools/{tool}/venv/bin/python"));
        }
    }
    fn setup(&self, workload: &str, extra: &[&str]) -> Output {
        let mut args = vec![
            "setup",
            "--agent",
            "pi",
            "--provider",
            "openrouter",
            "--model",
            "offline-fixture",
            "--workload",
            workload,
            "--output",
            "selected.toml",
        ];
        args.extend_from_slice(extra);
        self.command(&args)
    }
}
impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn trial_count_writes_a_sampled_sweep_with_a_frozen_replacement_pool() {
    let project = Project::new();
    project.seed_pi();
    project.write(
        "catalogs/agentbench/ASB-BUNDLE",
        "THUDM/AgentBench@d1e4a10db08c87075c78972e48ecc182be03e2d5\n",
    );
    project.write(
        "catalogs/agentbench/asb-trial/ASB-ADAPTER-FIXTURE",
        "fixture\n",
    );
    project.write("catalogs/agentbench.txt", "fixture workload\n");
    for id in ["one", "two", "three"] {
        project.write(
            &format!("catalogs/agentbench/source/configs/tasks/{id}.yaml"),
            "name: fixture\n",
        );
    }
    project.write("catalogs/selected.workload.toml", "workload='agentbench'\n[[trials]]\nid='one'\n[[trials]]\nid='two'\n[[trials]]\nid='three'\n");
    let result = project.setup(
        "agentbench",
        &[
            "--trial-set",
            "selected.workload.toml",
            "--trial-count",
            "2",
        ],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let text = std::fs::read_to_string(project.0.join("selected.toml")).unwrap();
    let sweep: toml::Value = toml::from_str(&text).unwrap();
    let configs = sweep["configs"].as_array().unwrap();
    assert_eq!(configs.len(), 2);
    let sample = &sweep["sampled_trials"].as_array().unwrap()[0];
    assert_eq!(sample["workload"].as_str(), Some("agentbench"));
    assert_eq!(sample["count"].as_integer(), Some(2));
    let candidates = sample["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 3);
    let actual = configs
        .iter()
        .map(|path| {
            let contents = std::fs::read_to_string(project.0.join(path.as_str().unwrap())).unwrap();
            let config: toml::Value = toml::from_str(&contents).unwrap();
            config["trial"].as_str().unwrap().to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        candidates[..2]
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    );
    let replay = project.command(&["sweep", "selected.toml", "--replay"]);
    assert!(!replay.status.success());
    assert!(String::from_utf8_lossy(&replay.stderr).contains("--prepare-replay"));
    assert!(!project.0.join("selected-results").exists());
    let original = std::fs::read(project.0.join("selected.toml")).unwrap();
    for count in ["0", "4", "not-a-number"] {
        let rejected = project.setup(
            "agentbench",
            &[
                "--trial-set",
                "selected.workload.toml",
                "--trial-count",
                count,
            ],
        );
        assert!(!rejected.status.success(), "accepted invalid count {count}");
        assert_eq!(
            original,
            std::fs::read(project.0.join("selected.toml")).unwrap()
        );
    }
}

#[test]
fn all_five_catalogs_accept_explicit_instances_and_reject_missing_or_unknown_before_writing() {
    for (workload, source, revision, split, population, sdk, version) in [
        (
            "swe-bench",
            "SWE-bench/SWE-bench",
            "c6fe717fd7a4c3ac1daa4055a4fd082c6a1d28a2",
            "test",
            2294,
            "swebench",
            "",
        ),
        (
            "swe-bench-lite",
            "SWE-bench/SWE-bench_Lite",
            "b0dde1093fe417d83b7184254edf8199c1f0dff5",
            "test",
            300,
            "swebench",
            "",
        ),
        (
            "swe-bench-verified",
            "SWE-bench/SWE-bench_Verified",
            "78f471bf655a3137b2e8a75af1501690ec009ec3",
            "test",
            500,
            "swebench",
            "",
        ),
        (
            "terminal-bench-1.0",
            "harbor-framework/terminal-bench-1",
            "91e10457b5410f16c44364da1a34cb6de8c488a5",
            "terminal-bench-core@0.1.1",
            80,
            "terminal-bench",
            "0.2.18",
        ),
        (
            "terminal-bench-2.0",
            "harbor-framework/terminal-bench-2",
            "2fd12b88aafdd04a52c298e3940bcb189f9766d6",
            "repository-root-tasks",
            89,
            "harbor",
            "0.24.0",
        ),
        (
            "terminal-bench-2.1",
            "harbor-framework/terminal-bench-2-1",
            "7131e4375048a0e408a8fb404b5f499d726b695b",
            "tasks",
            89,
            "harbor",
            "0.24.0",
        ),
    ] {
        let project = Project::new();
        project.seed_pi();
        project.seed_external_sdks();
        let tasks = (0..population).map(|index| {
            let id = format!("fixture-{index}");
            if sdk == "swebench" {
                let metadata = format!("evaluation-data/{revision}/{}.json","a".repeat(64));
                project.write(&format!("catalogs/{workload}/{metadata}"),"{}");
                json!({"id":id,"description":"synthetic selection only","repo":"fixture/repository",
                    "base_commit":"b".repeat(40),"metadata":metadata,"metadata_sha256":"c".repeat(64)})
            } else {
                // Terminal-Bench 2.0 keeps task definitions at the pinned
                // repository root; 1.0 and 2.1 use the `tasks/` subdirectory.
                let definition = if workload == "terminal-bench-2.0" {
                    format!("source/{id}")
                } else {
                    format!("source/tasks/{id}")
                };
                std::fs::create_dir_all(project.0.join("catalogs").join(workload).join(&definition)).unwrap();
                json!({"id":id,"description":"synthetic selection only","instruction":"fixture","definition":definition})
            }
        }).collect::<Vec<_>>();
        let mut catalog = json!({"schema_version":1,"workload":workload,
            "dataset":{"source":source,"revision":revision,"split":split},"tasks":tasks});
        if sdk != "swebench" {
            catalog["sdk"] = json!({"package":sdk,"version":version});
            catalog["source_sha256"] = json!("a".repeat(64));
            catalog["archive_sha256"] = json!("b".repeat(64));
            project.write(&format!("catalogs/{workload}/source-manifest.json"), "{}");
        }
        let path = format!("catalogs/{workload}/catalog.json");
        project.write(&path, &catalog.to_string());
        let arguments = if workload == "swe-bench" {
            vec!["--trial", "fixture-0", "--native-snapshot-record"]
        } else {
            vec!["--trial", "fixture-0"]
        };
        let result = project.setup(workload, &arguments);
        assert!(
            result.status.success(),
            "{workload}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let saved = std::fs::read(project.0.join("selected.toml")).unwrap();
        let config: toml::Value = toml::from_str(std::str::from_utf8(&saved).unwrap()).unwrap();
        assert_eq!(config["workload"].as_str(), Some(workload));
        assert_eq!(config["trial"].as_str(), Some("fixture-0"));
        assert_eq!(config["workspace"].as_str(), Some("workspaces"));
        if workload == "swe-bench" {
            assert_eq!(config["native_snapshot_record"].as_bool(), Some(true));
            assert!(
                String::from_utf8_lossy(&result.stdout).contains("asb run selected.toml --record")
            );
        } else {
            assert!(config.get("native_snapshot_record").is_none());
        }
        assert!(project.0.join("tools/pi/bin/pi").is_file());
        for arguments in [&[][..], &["--trial", "not-installed"][..]] {
            let result = project.setup(workload, arguments);
            assert!(!result.status.success(), "{workload}");
            assert_eq!(
                saved,
                std::fs::read(project.0.join("selected.toml")).unwrap()
            );
        }
        // A stale catalog cannot overwrite a previously usable configuration.
        project.write(&path, "{}");
        assert!(
            !project
                .setup(workload, &["--trial", "fixture-0"])
                .status
                .success()
        );
        assert_eq!(
            saved,
            std::fs::read(project.0.join("selected.toml")).unwrap()
        );
    }
}

#[test]
fn enabled_trial_sets_round_trip_and_invalid_selections_preserve_existing_config() {
    let project = Project::new();
    project.seed_pi();
    // Provide a local, pinned-layout AgentBench fixture: this test covers
    // trial-set selection, while external downloads are exercised elsewhere.
    project.write(
        "catalogs/agentbench/ASB-BUNDLE",
        "THUDM/AgentBench@d1e4a10db08c87075c78972e48ecc182be03e2d5\n",
    );
    project.write(
        "catalogs/agentbench/source/configs/tasks/one.yaml",
        "name: one\n",
    );
    project.write(
        "catalogs/agentbench/asb-trial/ASB-ADAPTER-FIXTURE",
        "fixture\n",
    );
    project.write("catalogs/agentbench.txt", "fixture workload\n");
    project.write(
        "catalogs/selected.workload.toml",
        "workload='agentbench'\n[[trials]]\nid='one'\ndescription='enabled fixture'\n",
    );
    let result = project.setup(
        "agentbench",
        &["--trial", "one", "--trial-set", "selected.workload.toml"],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let saved = std::fs::read(project.0.join("selected.toml")).unwrap();
    let config: toml::Value = toml::from_str(std::str::from_utf8(&saved).unwrap()).unwrap();
    assert_eq!(config["trial_set"].as_str(), Some("selected.workload.toml"));
    for arguments in [
        vec!["--trial", "one"],
        vec![
            "--trial",
            "disabled",
            "--trial-set",
            "selected.workload.toml",
        ],
        vec!["--trial", "one", "--trial-set", "../outside.toml"],
        vec!["--trial"],
    ] {
        assert!(!project.setup("agentbench", &arguments).status.success());
        assert_eq!(
            saved,
            std::fs::read(project.0.join("selected.toml")).unwrap()
        );
    }
}
