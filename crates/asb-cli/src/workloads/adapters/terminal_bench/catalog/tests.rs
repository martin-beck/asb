//! Offline source catalog integrity, immutable selections and install contracts.
use super::*;
use serde_json::{Value, json};
use std::path::PathBuf;

const ADAPTER: TerminalBenchAdapter = TerminalBenchAdapter {
    id: "fixture-suite",
    source: "fixture/source",
    revision: "pinned",
    split: "test",
    archive: "https://must-not-be-contacted.invalid/source.tar.gz",
    engine: crate::workloads::adapters::terminal_bench::ORIGINAL_ENGINE,
    version: "fixture-version",
    expected_tasks: 2,
    tasks_root: "tasks",
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root =
            crate::fixture_namespace::allocate(&std::env::temp_dir(), "asb-terminal-catalog-")
                .unwrap();
        fs::create_dir_all(root.join(".asb")).unwrap();
        fs::write(
            root.join(".asb/project.toml"),
            toml::to_string(&crate::project::ProjectConfig {
                tools_dir: "tools".into(),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        for task in ["first", "second"] {
            fs::create_dir_all(root.join(format!("workloads/fixture-suite/source/tasks/{task}")))
                .unwrap();
        }
        fs::write(
            root.join("workloads/fixture-suite/source-manifest.json"),
            "{}",
        )
        .unwrap();
        let fixture = Self(root.canonicalize().unwrap());
        fixture.write(&fixture.catalog());
        fixture
    }
    fn directory(&self) -> PathBuf {
        self.0.join("workloads")
    }
    fn catalog(&self) -> Value {
        json!({"schema_version":1,"workload":ADAPTER.id,
        "dataset":{"source":ADAPTER.source,"revision":ADAPTER.revision,"split":ADAPTER.split},
        "sdk":{"package":ADAPTER.engine.tool(),"version":ADAPTER.version},
        "source_sha256":"a".repeat(64),"archive_sha256":"b".repeat(64),
        "tasks":[
            {"id":"first","description":"First task","instruction":"private instruction A","definition":"source/tasks/first"},
            {"id":"second","description":"Second task","instruction":"private instruction B","definition":"source/tasks/second"}
        ]})
    }
    fn write(&self, value: &Value) {
        fs::write(
            self.directory().join("fixture-suite/catalog.json"),
            serde_json::to_vec(value).unwrap(),
        )
        .unwrap();
    }
    #[cfg(unix)]
    fn mock_runtime(&self, content: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let root = self.0.join("tools/terminal-bench");
        fs::create_dir_all(root.join("venv/bin")).unwrap();
        for name in ["python", "tb"] {
            let path = root.join("venv/bin").join(name);
            crate::write_test_file(&self.0, &path, content).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        root
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = crate::fixture_writer::cleanup(&self.0);
        #[cfg(not(unix))]
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn catalog_rejects_every_identity_and_task_integrity_mismatch() {
    let fixture = Fixture::new();
    assert_eq!(read(&ADAPTER, &fixture.directory()).unwrap().tasks.len(), 2);
    for pointer in [
        "/schema_version",
        "/workload",
        "/dataset/source",
        "/dataset/revision",
        "/dataset/split",
        "/sdk/package",
        "/sdk/version",
        "/source_sha256",
        "/archive_sha256",
        "/tasks/0/id",
        "/tasks/0/instruction",
        "/tasks/0/definition",
    ] {
        let mut value = fixture.catalog();
        *value.pointer_mut(pointer).unwrap() = if pointer == "/schema_version" {
            json!(99)
        } else {
            json!("")
        };
        fixture.write(&value);
        assert!(
            read(&ADAPTER, &fixture.directory()).is_err(),
            "accepted {pointer}"
        );
    }
    for mutate in [0, 1, 2, 3, 4] {
        let mut value = fixture.catalog();
        match mutate {
            0 => value["tasks"][1]["id"] = json!("first"),
            1 => value["tasks"][0]["id"] = json!("../escape"),
            2 => value["tasks"][0]["description"] = json!("x".repeat(1025)),
            3 => value["tasks"][0]["instruction"] = json!("x".repeat(65537)),
            _ => {
                value["tasks"].as_array_mut().unwrap().pop();
            }
        }
        fixture.write(&value);
        assert!(read(&ADAPTER, &fixture.directory()).is_err());
    }
    fixture.write(&fixture.catalog());
    fs::remove_file(
        fixture
            .directory()
            .join("fixture-suite/source-manifest.json"),
    )
    .unwrap();
    assert!(read(&ADAPTER, &fixture.directory()).is_err());
}

#[test]
fn catalog_rejects_oversize_symlink_and_escaped_task_sources() {
    let fixture = Fixture::new();
    let path = fixture.directory().join("fixture-suite/catalog.json");
    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(LIMIT + 1).unwrap();
    assert!(read(&ADAPTER, &fixture.directory()).is_err());
    fixture.write(&fixture.catalog());
    #[cfg(unix)]
    {
        let backup = path.with_extension("original");
        fs::rename(&path, &backup).unwrap();
        std::os::unix::fs::symlink(&backup, &path).unwrap();
        assert!(read(&ADAPTER, &fixture.directory()).is_err());
        fs::remove_file(&path).unwrap();
        fs::rename(backup, &path).unwrap();
        let task = fixture.directory().join("fixture-suite/source/tasks/first");
        fs::remove_dir(&task).unwrap();
        std::os::unix::fs::symlink(&fixture.0, &task).unwrap();
        assert!(read(&ADAPTER, &fixture.directory()).is_err());
    }
}

#[test]
fn selection_preserves_valid_subset_and_backs_up_incompatible_previous_content() {
    let fixture = Fixture::new();
    let catalog = read(&ADAPTER, &fixture.directory()).unwrap();
    let directory = fixture.directory();
    let path = directory.join("fixture-suite.workload.toml");
    let previous = "workload='fixture-suite'\ntrials=[{id='second',description='my subset'}]\n";
    fs::write(&path, previous).unwrap();
    selection(ADAPTER.id, &catalog.tasks, &directory, &mut Vec::new()).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), previous);
    fs::write(&path, "workload='wrong'\ntrials=[]\n").unwrap();
    let mut output = Vec::new();
    selection(ADAPTER.id, &catalog.tasks, &directory, &mut output).unwrap();
    assert!(String::from_utf8(output).unwrap().contains("Preserved"));
    let selected: TrialSet = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(selected.workload, ADAPTER.id);
    assert_eq!(selected.trials.len(), 2);
    assert!(fs::read_dir(&directory).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("backup-")
    }));
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(selection(ADAPTER.id, &catalog.tasks, &directory, &mut Vec::new()).is_err());
}

#[cfg(unix)]
#[test]
fn local_install_verifies_runtime_and_catalog_without_fetching_or_replacing_subset() {
    let fixture = Fixture::new();
    let tool = fixture.mock_runtime("#!/bin/sh\nexit 0\n");
    let context = InstallationContext {
        process: &crate::install::OperationContext::new(
            &fixture.0,
            &crate::project::ProjectConfig::default(),
        )
        .unwrap(),
        project_root: &fixture.0,
        tools_root: &fixture.0.join("tools"),
        workloads_root: &fixture.directory(),
    };
    let mut progress = Vec::new();
    let mut output = Vec::new();
    install(&ADAPTER, &context, &mut output, &mut |percent, message| {
        progress.push((percent, message.to_owned()))
    })
    .unwrap();
    assert_eq!(progress.last().unwrap().0, 100);
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("2 official tasks")
    );
    assert!(fixture.directory().join("fixture-suite.txt").is_file());
    assert!(
        fixture
            .directory()
            .join("fixture-suite.workload.toml")
            .is_file()
    );
    for file in [
        "asb_terminal_catalog.py",
        "terminal_bench_1_tasks.json",
        "asb_terminal_bench_1.py",
        "asb_harbor.py",
    ] {
        assert!(tool.join(file).is_file());
    }
    assert!(
        !fixture.0.join(".asb-cache").exists(),
        "verified local catalogs must not download"
    );
    assert_eq!(
        crate::install::OperationContext::load(&tool)
            .unwrap()
            .root(),
        fixture.0,
        "legacy runtime lookup must resolve this initialized private project"
    );
    #[cfg(target_os = "linux")]
    let diagnostic_count = || {
        fs::read_dir(&fixture.0)
            .unwrap()
            .filter(|entry| {
                let entry = entry.as_ref().unwrap();
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".asb-install-step-")
                    && entry.path().join("output.log").is_file()
            })
            .count()
    };
    #[cfg(target_os = "linux")]
    let before = diagnostic_count();
    assert!(runtime_available(&ADAPTER, &tool).unwrap());
    crate::write_test_file(
        &fixture.0,
        &tool.join("venv/bin/python"),
        "#!/bin/sh\nexit 7\n",
    )
    .unwrap();
    assert!(!runtime_available(&ADAPTER, &tool).unwrap());
    fs::write(
        tool.join("venv/bin/python"),
        "#!/bin/sh\nprintf started > \"$0.started\"\nexit 127\n",
    )
    .unwrap();
    assert!(!runtime_available(&ADAPTER, &tool).unwrap());
    assert_eq!(
        fs::read(tool.join("venv/bin/python.started")).unwrap(),
        b"started"
    );
    fs::remove_file(tool.join("venv/bin/python")).unwrap();
    let missing = runtime_available(&ADAPTER, &tool).unwrap_err().to_string();
    #[cfg(target_os = "linux")]
    {
        assert!(missing.contains("cannot inspect installed evaluator runtime"));
        assert!(missing.contains(&tool.join("venv/bin/python").display().to_string()));
    }
    #[cfg(not(target_os = "linux"))]
    assert!(!missing.is_empty()); // Keep native direct-exec error semantics.
    #[cfg(target_os = "linux")]
    assert_eq!(
        diagnostic_count() - before,
        3,
        "all three legacy probe diagnostics remain fixture-owned; the missing executable never launches"
    );
}

#[cfg(unix)]
#[test]
fn rejected_cached_catalog_validation_is_actionable_without_network() {
    let fixture = Fixture::new();
    fixture.mock_runtime("#!/bin/sh\ncase \"$2\" in verify|install) exit 7;; *) exit 0;; esac\n");
    let cache = fixture.0.join(".asb-cache/workload-sources");
    fs::create_dir_all(&cache).unwrap();
    let archive = cache.join("fixture-suite-pinned.tar.gz");
    fs::write(&archive, "fixture cached archive validated by mock runtime").unwrap();
    let context = InstallationContext {
        process: &crate::install::OperationContext::new(
            &fixture.0,
            &crate::project::ProjectConfig::default(),
        )
        .unwrap(),
        project_root: &fixture.0,
        tools_root: &fixture.0.join("tools"),
        workloads_root: &fixture.directory(),
    };
    let error = install(&ADAPTER, &context, &mut Vec::new(), &mut |_, _| {})
        .unwrap_err()
        .to_string();
    assert!(error.contains("dataset validation failed"), "{error}");
    assert!(error.contains("asb tool install terminal-bench"), "{error}");
    assert!(
        archive.is_file(),
        "failed validation must preserve cached evidence"
    );
}
