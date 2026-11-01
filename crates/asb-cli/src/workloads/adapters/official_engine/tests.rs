//! Actual offline helpers, not an installed evaluator or a live environment.
use super::*;
use crate::workloads::recovery::{AttemptReference, RecoveryContext, RetainedOutcomeContext};
use crate::workloads::session::DatasetIdentity;
use serde_json::json;
use std::fs;
use std::path::PathBuf;

struct Fixture {
    root: PathBuf,
    project: crate::project::ProjectConfig,
    reference: AttemptReference,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("engine", None).attempt_id);
        fs::create_dir_all(root.join(".asb/attempts/a-b-c")).unwrap();
        let root = root.canonicalize().unwrap();
        let reference = AttemptReference {
            schema_version: 1,
            preparation_id: "a-b-c".into(),
            workload: "fixture-suite".into(),
            task_id: "task-0".into(),
            dataset: DatasetIdentity {
                source: "fixture/data".into(),
                revision: "b".repeat(40),
                split: "test".into(),
            },
            artifacts: root.join(".asb/attempts/a-b-c"),
        };
        Self {
            root,
            project: crate::project::ProjectConfig {
                tools_dir: "custom-tools".into(),
                ..Default::default()
            },
            reference,
        }
    }
    fn context(&self) -> RecoveryContext<'_> {
        RecoveryContext {
            project_root: &self.root,
            project_config: &self.project,
            reference: &self.reference,
        }
    }
    fn sdk(&self) -> PathBuf {
        self.root.join("custom-tools/fixture-sdk")
    }
    fn install_inert_sdk(&self) {
        let python = self.sdk().join("venv/bin/python");
        fs::create_dir_all(python.parent().unwrap()).unwrap();
        // The failure-order cases must stop before trying this non-executable.
        fs::write(python, "never execute").unwrap();
    }
    fn private_runtime(&self) -> PathBuf {
        PathBuf::from(fs::read_to_string(self.root.join(".asb/published-runtime")).unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn record_publication(root: &Path) -> Result<(), Error> {
    assert!(
        root.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("grade-inspection-")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    let marker = root.parent().unwrap().join("published-runtime");
    assert!(!marker.exists(), "publisher called more than once");
    fs::write(marker, root.to_str().unwrap()).map_err(crate::io_error)
}
fn failing_publisher(root: &Path) -> Result<(), Error> {
    record_publication(root)?;
    Err(Error::Input("fixture helper publication failed".into()))
}
fn successful_publisher(root: &Path) -> Result<(), Error> {
    record_publication(root)?;
    // Exercise exact command/module routing and bounded actual controller I/O.
    // The helper produces protocol facts only, not real grading evidence.
    fs::write(root.join("asb_retained.py"),r#"import json,sys
from pathlib import Path
assert sys.argv[1:3] == ['--module','fixture_engine']
assert sys.argv[3] in ['--inspect-grade','--grade']
request=json.loads(sys.stdin.readline())
ref=request['reference']
Path(__file__).parent.parent.joinpath('request.json').write_text(json.dumps(request))
print(json.dumps({'schema_version':1,'scope':'retained-official-grade-not-execution-recovery',
'task':dict(workload=ref['workload'],task_id=ref['task_id'],dataset=ref['dataset'],attempt_id='asb-'+'a'*32),
'workload_baseline':{'kind':'upstream-task-definition','base_commit':'b'*40},
'workload_evaluation':'upstream-harness','workload_success':False,'recorded_cleanup':'stopped-preserved',
'evidence':{'task.json':{'sha256':'d'*64,'bytes':100}},
'process_success':None,'exit_code':None,'elapsed_ms':None,'token_usage':None,'steps':None}),flush=True)
"#).map_err(crate::io_error)
}

#[test]
fn three_owner_bindings_route_exact_engines_and_read_only_runtime_inputs() {
    for (binding, tool, module) in [
        (super::super::swe_bench::ENGINE, "swebench", "asb_swebench"),
        (
            super::super::terminal_bench::ORIGINAL_ENGINE,
            "terminal-bench",
            "asb_terminal_bench_1",
        ),
        (
            super::super::terminal_bench::HARBOR_ENGINE,
            "harbor",
            "asb_harbor",
        ),
    ] {
        assert_eq!(binding.tool(), tool);
        assert_eq!(binding.module, module);
        assert_eq!(binding.script(), format!("{module}.py"));
        let fixture = Fixture::new();
        let inputs = binding
            .runtime_inputs(&crate::runtime_inputs::Context {
                root: &fixture.root,
                invocation_dir: &fixture.root,
                project: &fixture.project,
            })
            .unwrap();
        assert!(
            inputs.contains(&crate::runtime_inputs::Dependency::Python {
                path: fixture
                    .root
                    .join("custom-tools")
                    .join(tool)
                    .join("venv/bin/python"),
                site: true
            })
        );
        assert!(!fixture.root.join("custom-tools").exists());
        assert_eq!(fs::read_dir(fixture.root.join(".asb")).unwrap().count(), 1);
    }
}

#[test]
fn inspection_validates_location_then_sdk_before_any_helper_publication() {
    let binding = EngineBinding::new("fixture-sdk", "fixture_engine", failing_publisher);
    let mut fixture = Fixture::new();
    fixture.reference.artifacts = fixture.root.join("elsewhere");
    assert!(
        binding
            .inspect_grade("fixture-suite", &fixture.context())
            .unwrap_err()
            .to_string()
            .contains("outside its project")
    );
    fixture.reference.artifacts = fixture.root.join(".asb/attempts/a-b-c");
    assert!(
        binding
            .inspect_grade("fixture-suite", &fixture.context())
            .unwrap_err()
            .to_string()
            .contains("asb workload install fixture-suite")
    );
    assert!(!fixture.root.join(".asb/published-runtime").exists());
    assert_eq!(fs::read_dir(fixture.root.join(".asb")).unwrap().count(), 1);
}

#[test]
fn helper_failure_prevents_request_and_removes_only_its_private_runtime() {
    let binding = EngineBinding::new("fixture-sdk", "fixture_engine", failing_publisher);
    for grading in [false, true] {
        let fixture = Fixture::new();
        fixture.install_inert_sdk();
        let unrelated = fixture.root.join(".asb/grade-inspection-unrelated");
        fs::create_dir(&unrelated).unwrap();
        fs::write(unrelated.join("sentinel"), "keep").unwrap();
        let result = if grading {
            binding.grade_retained(
                &fixture.context(),
                &fixture.root.join("owner-validated-catalog.json"),
            )
        } else {
            binding.inspect_grade("fixture-suite", &fixture.context())
        };
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("fixture helper publication failed")
        );
        assert!(!fixture.private_runtime().exists());
        assert_eq!(
            fs::read_to_string(unrelated.join("sentinel")).unwrap(),
            "keep"
        );
        assert!(!fixture.root.join(".asb/request.json").exists());
    }
}

#[test]
fn preflight_missing_sdk_and_helper_failure_precede_guest_bootstrap() {
    let fixture = Fixture::new();
    let binding = EngineBinding::new("fixture-sdk", "fixture_engine", |root| {
        fs::write(root.join("publisher-called"), "once").map_err(crate::io_error)?;
        Err(Error::Input(
            "fixture publication prevents bootstrap".into(),
        ))
    });
    let context = PreflightContext {
        project_root: &fixture.root,
        project_config: &fixture.project,
        agent: "missing-fixture-agent",
    };
    assert!(
        binding
            .preflight("fixture-suite", &context)
            .unwrap_err()
            .to_string()
            .contains("official evaluator")
    );
    assert!(!fixture.sdk().exists());
    fixture.install_inert_sdk();
    assert!(
        binding
            .preflight("fixture-suite", &context)
            .unwrap_err()
            .to_string()
            .contains("fixture publication prevents bootstrap")
    );
    assert!(fixture.sdk().join("publisher-called").is_file());
}

#[test]
fn verify_and_recover_do_not_publish_new_helpers() {
    let fixture = Fixture::new();
    fixture.install_inert_sdk();
    let binding = EngineBinding::new("fixture-sdk", "fixture_engine", |_| {
        panic!("verify/recover must not publish helpers")
    });
    assert!(
        binding
            .recover(&fixture.context())
            .unwrap_err()
            .to_string()
            .contains("recovery helper is missing")
    );
    assert!(
        binding
            .verify_retained(&RetainedOutcomeContext {
                recovery: fixture.context(),
                summary: &json!({})
            })
            .is_err()
    );
    assert_eq!(fs::read_dir(fixture.root.join(".asb")).unwrap().count(), 1);
}

#[test]
#[cfg(unix)]
fn actual_private_helper_preserves_false_unknown_metrics_and_exact_catalog_argument() {
    use std::os::unix::fs::symlink;
    // Resolve only an interpreter for a synthetic standard-library helper.
    let python = crate::tools::host_executable(Path::new("python3")).unwrap();
    for grading in [false, true] {
        let fixture = Fixture::new();
        let interpreter = fixture.sdk().join("venv/bin/python");
        fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
        symlink(&python, &interpreter).unwrap();
        let binding = EngineBinding::new("fixture-sdk", "fixture_engine", successful_publisher);
        let catalog = fixture.root.join("owner-validated-catalog.json");
        let report = if grading {
            binding.grade_retained(&fixture.context(), &catalog)
        } else {
            binding.inspect_grade("fixture-suite", &fixture.context())
        }
        .unwrap();
        assert_eq!(report["workload_success"], false);
        assert!(report["elapsed_ms"].is_null());
        assert!(!fixture.private_runtime().exists());
        let request: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.root.join(".asb/request.json")).unwrap())
                .unwrap();
        assert_eq!(
            request["catalog"],
            if grading { json!(catalog) } else { json!(null) }
        );
        assert_eq!(
            request["reference"],
            serde_json::to_value(&fixture.reference).unwrap()
        );
    }
}

#[test]
#[cfg(unix)]
fn invalid_private_helper_report_is_not_a_grade_and_runtime_is_removed() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let python = crate::tools::host_executable(Path::new("python3")).unwrap();
    let interpreter = fixture.sdk().join("venv/bin/python");
    fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
    symlink(python, interpreter).unwrap();
    let binding = EngineBinding::new("fixture-sdk", "fixture_engine", |root| {
        successful_publisher(root)?;
        let helper = root.join("asb_retained.py");
        let source = fs::read_to_string(&helper).map_err(crate::io_error)?;
        fs::write(
            helper,
            source.replace("'workload_success':False", "'workload_success':None"),
        )
        .map_err(crate::io_error)
    });
    assert!(
        binding
            .inspect_grade("fixture-suite", &fixture.context())
            .unwrap_err()
            .to_string()
            .contains("invalid or mismatched report")
    );
    assert!(!fixture.private_runtime().exists());
}

#[test]
fn three_swe_owners_reject_dataset_mismatch_before_helpers_or_sdk_admission() {
    use crate::workloads::adapters::WorkloadAdapter;
    for (adapter, count) in [
        (&super::super::swe_bench::ADAPTER, 2294),
        (&super::super::swe_bench_lite::ADAPTER, 300),
        (&super::super::swe_bench_verified::ADAPTER, 500),
    ] {
        let mut fixture = Fixture::new();
        let installed = fixture.root.join("workloads").join(adapter.id);
        let metadata = format!("evaluation-data/{}/task.json", adapter.revision);
        fs::create_dir_all(installed.join(&metadata).parent().unwrap()).unwrap();
        fs::write(installed.join(&metadata), "{}").unwrap();
        let dataset = DatasetIdentity {
            source: adapter.dataset.into(),
            revision: adapter.revision.into(),
            split: "test".into(),
        };
        let tasks = (0..count).map(|index| json!({"id":format!("task-{index}"),"description":"fixture task", "repo":"owner/repo","base_commit":"b".repeat(40),"metadata":metadata,"metadata_sha256":"c".repeat(64)})).collect::<Vec<_>>();
        fs::write(
            installed.join("catalog.json"),
            serde_json::to_vec(
                &json!({"schema_version":1,"workload":adapter.id,"dataset":dataset,"tasks":tasks}),
            )
            .unwrap(),
        )
        .unwrap();
        fixture.reference.workload = adapter.id.into();
        fixture.reference.dataset = dataset.clone();
        fixture.reference.dataset.revision = "wrong".into();
        assert!(
            adapter
                .grade_retained(&fixture.context())
                .unwrap_err()
                .to_string()
                .contains("retained task dataset differs from the installed grading catalog")
        );
        assert_eq!(fs::read_dir(fixture.root.join(".asb")).unwrap().count(), 1);
        fixture.reference.dataset = dataset;
        for error in [
            adapter.grade_retained(&fixture.context()).unwrap_err(),
            adapter.inspect_grade(&fixture.context()).unwrap_err(),
            adapter.recover(&fixture.context()).unwrap_err(),
        ] {
            assert!(error.to_string().contains("official evaluator"), "{error}");
        }
        assert_eq!(fs::read_dir(fixture.root.join(".asb")).unwrap().count(), 1);
        assert_eq!(
            adapter.process_lifetime(),
            crate::workloads::session::ProcessLifetime::Quiesce
        );
    }
}
