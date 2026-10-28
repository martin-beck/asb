//! Catalog integrity and isolated SDK-orchestration fixtures, not official SDK
//! or upstream dataset qualification. No downloader is on the child PATH.
use super::*;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Command;

#[cfg(unix)]
#[test]
fn isolated_catalog_install_orchestration_and_incomplete_population_preservation() {
    use std::os::unix::fs::PermissionsExt;
    const CASE: &str = "ASB_SWE_CATALOG_INSTALL_CASE";
    let Ok(case) = std::env::var(CASE) else {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/swe-catalog-install-fixtures")
            .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let name = format!(
            "{}::isolated_catalog_install_orchestration_and_incomplete_population_preservation",
            module_path!().split_once("::").unwrap().1
        );
        let python = crate::tools::host_executable(Path::new("python3")).unwrap();
        for case in [
            "existing",
            "missing",
            "changed",
            "verify-fails",
            "identity-fails",
            "catalog-fails",
            "invalid-materialized",
            "linked-bundle",
            "label-blocked",
        ] {
            let directory = root.join(case);
            fs::create_dir_all(directory.join("commands")).unwrap();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .current_dir(&directory)
                .env(CASE, case)
                .env("ASB_SWE_CATALOG_PYTHON", &python)
                .env("PATH", directory.join("commands"))
                .output()
                .unwrap();
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
    let fixture = Fixture::new();
    let tools = fixture.0.join("tools");
    let tool = tools.join("swebench");
    fs::create_dir_all(tool.join("venv/bin")).unwrap();
    fs::write(tool.join("venv/bin/swebench"), "fixture installed marker").unwrap();
    fs::set_permissions(
        tool.join("venv/bin/swebench"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    assert!(
        crate::install::tool_installed_locally("swebench", &tools),
        "fixture must prevent any SDK installation"
    );
    let payload = fixture.0.join("saved-dataset");
    fs::create_dir_all(&payload).unwrap();
    fs::rename(fixture.0.join(ADAPTER.id), payload.join(ADAPTER.id)).unwrap();
    if case == "existing" || case == "verify-fails" || case == "label-blocked" {
        fs::create_dir_all(
            fixture
                .0
                .join(ADAPTER.id)
                .join("evaluation-data")
                .join(ADAPTER.revision),
        )
        .unwrap();
        fs::write(fixture.metadata(), "{}").unwrap();
        fixture.write(&fixture.catalog());
    }
    if case == "changed" {
        fs::create_dir_all(fixture.0.join(ADAPTER.id)).unwrap();
        fs::write(
            fixture.0.join(ADAPTER.id).join("original-evidence"),
            "keep this",
        )
        .unwrap();
    }
    if case == "linked-bundle" {
        std::os::unix::fs::symlink(payload.join(ADAPTER.id), fixture.0.join(ADAPTER.id)).unwrap();
    }
    if case == "label-blocked" {
        fs::create_dir(fixture.0.join("fixture-suite.txt")).unwrap();
    }
    let python = PathBuf::from(std::env::var_os("ASB_SWE_CATALOG_PYTHON").unwrap());
    let code = format!(
        r#"#!{}
import hashlib,os,sys,shutil,json
from pathlib import Path
mode=os.environ['ASB_SWE_CATALOG_INSTALL_CASE']
payload=Path({})
name=Path(sys.argv[1]).name
if name=='asb_swebench_sources.py':
 archive=Path(sys.argv[sys.argv.index('--archive')+1])
 assert hashlib.sha256(archive.read_bytes()).digest()==hashlib.sha256(b'fixture source bytes').digest()
 sys.exit(0)
operation=sys.argv[2]
if operation=='identity': sys.exit(2 if mode=='identity-fails' else 0)
if operation=='verify': sys.exit(2 if mode in ('verify-fails','linked-bundle') else 0)
assert operation=='catalog'
if mode=='catalog-fails': sys.exit(2)
destination=Path(sys.argv[sys.argv.index('--output')+1])
shutil.copytree(payload/'fixture-suite',destination)
if mode=='invalid-materialized': (destination/'catalog.json').write_text('invalid catalog')
"#,
        python.display(),
        serde_json::to_string(&payload.to_string_lossy()).unwrap()
    );
    let executable = tool.join("venv/bin/python");
    fs::write(&executable, &code).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let source_archive = "swe-bench-tasks-3d07b464b7b311a0cbfb5ed5b2d8a3b96f84a33d.tar.gz";
    fs::write(tool.join(source_archive), b"fixture source bytes").unwrap();
    let context = InstallationContext {
        process: &crate::install::OperationContext::new(
            &fixture.0,
            &crate::project::ProjectConfig::default(),
        )
        .unwrap(),
        project_root: &fixture.0,
        tools_root: &tools,
        workloads_root: &fixture.0,
    };
    let mut output = Vec::new();
    let mut progress = Vec::new();
    let result = install(&ADAPTER, &context, &mut output, &mut |percent, _| {
        progress.push(percent)
    });
    assert_eq!(
        fs::read_to_string(&executable).unwrap(),
        code,
        "fixture SDK interpreter must never be reinstalled"
    );
    assert!(
        !fixture.0.join(".asb-cache/workload-sources").exists(),
        "prevalidated fixture source must never trigger downloading"
    );
    if matches!(
        case.as_str(),
        "existing" | "missing" | "changed" | "verify-fails"
    ) {
        result.unwrap();
        assert!(progress.contains(&100));
        assert!(fixture.0.join("fixture-suite.txt").is_file());
        let selected: crate::workloads::TrialSet = toml::from_str(
            &fs::read_to_string(fixture.0.join("fixture-suite.workload.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(selected.trials.len(), 2);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("2 official test instances"));
        if case == "changed" {
            assert!(output.contains("Preserved the incomplete or changed dataset"));
            let backup = fs::read_dir(&fixture.0)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("fixture-suite.asb-backup-")
                })
                .unwrap();
            assert_eq!(
                fs::read_to_string(backup.join("original-evidence")).unwrap(),
                "keep this"
            );
        }
    } else {
        let error = result.unwrap_err().to_string();
        let expected = match case.as_str() {
            "identity-fails" => "does not match its recipe",
            "catalog-fails" => "official dataset installation failed",
            "invalid-materialized" => "incorrect JSON schema",
            "linked-bundle" => "must not be a symlink",
            "label-blocked" => "cannot write workload catalog label",
            _ => unreachable!(),
        };
        assert!(error.contains(expected), "{error}");
    }
}

const ADAPTER: SweBenchAdapter = SweBenchAdapter {
    id: "fixture-suite",
    dataset: "fixture/source",
    revision: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    source_sha256: "fcd30fba8098151a003ce953022e7e6db16f53f82298290a285b22722483a5a1",
    expected_tasks: 2,
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/swe-catalog-fixtures")
            .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
        fs::create_dir_all(
            root.join("fixture-suite/evaluation-data")
                .join(ADAPTER.revision),
        )
        .unwrap();
        let fixture = Self(root);
        fs::write(fixture.metadata(), "{}").unwrap();
        fixture.write(&fixture.catalog());
        fixture
    }
    fn metadata(&self) -> PathBuf {
        self.0
            .join("fixture-suite/evaluation-data")
            .join(ADAPTER.revision)
            .join("task.json")
    }
    fn catalog_path(&self) -> PathBuf {
        self.0.join("fixture-suite/catalog.json")
    }
    fn catalog(&self) -> Value {
        let task = |id| {
            json!({"id":id,"description":"Fix the fixture","repo":"owner/repo","base_commit":"b".repeat(40),
        "metadata":format!("evaluation-data/{}/task.json",ADAPTER.revision),"metadata_sha256":"c".repeat(64)})
        };
        json!({"schema_version":1,"workload":ADAPTER.id,"dataset":{"source":ADAPTER.dataset,"revision":ADAPTER.revision,"split":"test"},
            "tasks":[task("first"),task("second")]})
    }
    fn write(&self, value: &Value) {
        fs::write(self.catalog_path(), serde_json::to_vec(value).unwrap()).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn catalog_rejects_schema_population_identity_and_path_corruption() {
    let fixture = Fixture::new();
    assert_eq!(read(&ADAPTER, &fixture.0).unwrap().tasks.len(), 2);
    for (pointer, value) in [
        ("/schema_version", json!(9)),
        ("/workload", json!("wrong")),
        ("/dataset/source", json!("wrong")),
        ("/dataset/revision", json!("wrong")),
        ("/dataset/split", json!("train")),
        ("/tasks/0/id", json!("")),
        ("/tasks/0/id", json!("x".repeat(257))),
        ("/tasks/0/id", json!("../escape")),
        ("/tasks/0/description", json!("x".repeat(1025))),
        ("/tasks/0/repo", json!("")),
        ("/tasks/0/repo", json!("x".repeat(257))),
        ("/tasks/0/base_commit", json!("g".repeat(40))),
        ("/tasks/0/base_commit", json!("a".repeat(39))),
        ("/tasks/0/metadata_sha256", json!("g".repeat(64))),
        ("/tasks/0/metadata_sha256", json!("a".repeat(63))),
        ("/tasks/0/metadata", json!("../outside.json")),
        (
            "/tasks/0/metadata",
            json!(format!("evaluation-data/{}/task.txt", ADAPTER.revision)),
        ),
        (
            "/tasks/0/metadata",
            json!(format!(
                "evaluation-data/{}/nested/task.json",
                ADAPTER.revision
            )),
        ),
        (
            "/tasks/0/metadata",
            json!(format!("evaluation-data/{}/../task.json", ADAPTER.revision)),
        ),
    ] {
        let mut changed = fixture.catalog();
        *changed.pointer_mut(pointer).unwrap() = value;
        fixture.write(&changed);
        assert!(read(&ADAPTER, &fixture.0).is_err(), "accepted {pointer}");
    }
    for index in 0..3 {
        let mut changed = fixture.catalog();
        match index {
            0 => changed["tasks"][1]["id"] = json!("first"),
            1 => {
                changed["tasks"].as_array_mut().unwrap().pop();
            }
            _ => changed["unexpected"] = json!(true),
        }
        fixture.write(&changed);
        assert!(read(&ADAPTER, &fixture.0).is_err());
    }
}
#[test]
fn catalog_files_and_metadata_are_bounded_and_stay_within_installed_dataset() {
    let fixture = Fixture::new();
    let catalog = fixture.catalog_path();
    fs::write(&catalog, b"not json").unwrap();
    assert!(
        read(&ADAPTER, &fixture.0)
            .err()
            .unwrap()
            .contains("incorrect JSON schema")
    );
    let file = fs::OpenOptions::new().write(true).open(&catalog).unwrap();
    file.set_len(MAX_CATALOG_BYTES + 1).unwrap();
    drop(file);
    assert!(
        read(&ADAPTER, &fixture.0)
            .err()
            .unwrap()
            .contains("exceeds 16 MiB")
    );
    fixture.write(&fixture.catalog());
    fs::remove_file(fixture.metadata()).unwrap();
    assert!(
        read(&ADAPTER, &fixture.0)
            .err()
            .unwrap()
            .contains("metadata is missing")
    );
    fs::create_dir(fixture.metadata()).unwrap();
    assert!(
        read(&ADAPTER, &fixture.0)
            .err()
            .unwrap()
            .contains("not a bounded local file")
    );
    fs::remove_dir(fixture.metadata()).unwrap();
    fs::write(fixture.metadata(), "{}").unwrap();
    let file = fs::OpenOptions::new()
        .write(true)
        .open(fixture.metadata())
        .unwrap();
    file.set_len(4 * 1024 * 1024 + 1).unwrap();
    drop(file);
    assert!(
        read(&ADAPTER, &fixture.0)
            .err()
            .unwrap()
            .contains("not a bounded local file")
    );
    #[cfg(unix)]
    {
        fs::remove_file(fixture.metadata()).unwrap();
        let outside = fixture.0.join("outside.json");
        fs::write(&outside, "{}").unwrap();
        std::os::unix::fs::symlink(&outside, fixture.metadata()).unwrap();
        assert!(read(&ADAPTER, &fixture.0).is_err());
        fs::remove_file(fixture.metadata()).unwrap();
        let original = catalog.with_extension("original");
        fs::rename(&catalog, &original).unwrap();
        std::os::unix::fs::symlink(&original, &catalog).unwrap();
        assert!(
            read(&ADAPTER, &fixture.0)
                .err()
                .unwrap()
                .contains("catalog must not be a symlink")
        );
    }
}
#[test]
fn selection_preserves_exact_subsets_and_never_overwrites_backup_evidence() {
    let fixture = Fixture::new();
    let catalog = read(&ADAPTER, &fixture.0).unwrap();
    let path = fixture.0.join("fixture-suite.workload.toml");
    let selected=b"# user's exact bytes\nworkload='fixture-suite'\ntrials=[{id='second',description='custom'}]\n";
    fs::write(&path, selected).unwrap();
    publish_selection(&ADAPTER, &catalog, &fixture.0, &mut Vec::new()).unwrap();
    assert_eq!(fs::read(&path).unwrap(), selected);
    let previous_backup = path.with_extension("toml.legacy-1");
    fs::write(&previous_backup, "older retained evidence").unwrap();
    for previous in [
        b"not toml".as_slice(),
        b"\xff\xfe",
        b"workload='wrong'\ntrials=[]",
        b"workload='fixture-suite'\ntrials=[]",
        b"workload='fixture-suite'\ntrials=[{id='first'},{id='first'}]",
        b"workload='fixture-suite'\ntrials=[{id='missing'}]",
    ] {
        fs::write(&path, previous).unwrap();
        let mut output = Vec::new();
        publish_selection(&ADAPTER, &catalog, &fixture.0, &mut output).unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Preserved the previous incompatible")
        );
        let loaded: crate::workloads::TrialSet =
            toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.trials.len(), 2);
    }
    assert_eq!(
        fs::read_to_string(previous_backup).unwrap(),
        "older retained evidence"
    );
    assert_eq!(
        fs::read(path.with_extension("toml.legacy-2")).unwrap(),
        b"not toml"
    );
}
#[test]
fn selection_refuses_oversized_and_linked_inputs_and_missing_destinations() {
    let fixture = Fixture::new();
    let catalog = read(&ADAPTER, &fixture.0).unwrap();
    let path = fixture.0.join("fixture-suite.workload.toml");
    let file = fs::File::create(&path).unwrap();
    file.set_len(MAX_CATALOG_BYTES + 1).unwrap();
    drop(file);
    assert!(
        publish_selection(&ADAPTER, &catalog, &fixture.0, &mut Vec::new())
            .unwrap_err()
            .to_string()
            .contains("exceeds 16 MiB")
    );
    fs::remove_file(&path).unwrap();
    #[cfg(unix)]
    {
        let target = fixture.0.join("unrelated");
        fs::write(&target, "must remain unchanged").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(
            publish_selection(&ADAPTER, &catalog, &fixture.0, &mut Vec::new())
                .unwrap_err()
                .to_string()
                .contains("must not be a symlink")
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "must remain unchanged");
        fs::remove_file(&path).unwrap();
    }
    assert!(
        publish_selection(
            &ADAPTER,
            &catalog,
            &fixture.0.join("missing"),
            &mut Vec::new()
        )
        .is_err()
    );
    let helper_root = fixture.0.join("helpers");
    fs::create_dir(&helper_root).unwrap();
    ensure_helpers(&helper_root).unwrap();
    for name in [
        "asb_swebench.py",
        "asb_swebench_task.py",
        "asb_swebench_sources.py",
        "asb_retained.py",
    ] {
        assert!(helper_root.join(name).is_file());
    }
}
