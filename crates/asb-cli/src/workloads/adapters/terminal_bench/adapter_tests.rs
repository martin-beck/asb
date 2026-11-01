//! Offline adapter boundary tests: no SDK, Docker, provider, or task is run.
use super::*;
use crate::workloads::adapters::WorkloadAdapter;
use crate::workloads::recovery::{AttemptReference, RecoveryContext, RetainedOutcomeContext};
use crate::workloads::session::{DatasetIdentity, PreparationContext, ProcessLifetime};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

struct Fixture {
    root: PathBuf,
    project: crate::project::ProjectConfig,
}
impl Fixture {
    fn new(adapter: &TerminalBenchAdapter) -> Self {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/terminal-adapter-fixtures")
            .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
        let project = crate::project::ProjectConfig {
            tools_dir: "tools".into(),
            workloads_dir: "workloads".into(),
            ..Default::default()
        };
        fs::create_dir_all(root.join(".asb/attempts")).unwrap();
        fs::write(
            root.join(".asb/project.toml"),
            toml::to_string(&project).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(root.join("workspace")).unwrap();
        let root = root.canonicalize().unwrap();
        let installed = root.join("workloads").join(adapter.id);
        let tasks = (0..adapter.expected_tasks).map(|index| {
            fs::create_dir_all(installed.join(format!("source/tasks/task-{index}"))).unwrap();
            json!({"id":format!("task-{index}"),"description":format!("Task {index}"),"instruction":"Fixture instruction","definition":format!("source/tasks/task-{index}")})
        }).collect::<Vec<_>>();
        fs::write(installed.join("source-manifest.json"), "{}").unwrap();
        fs::write(installed.join("catalog.json"), serde_json::to_vec(&json!({
            "schema_version":1,"workload":adapter.id,
            "dataset":{"source":adapter.source,"revision":adapter.revision,"split":adapter.split},
            "sdk":{"package":adapter.engine.tool(),"version":adapter.version},
            "source_sha256":"a".repeat(64),"archive_sha256":"b".repeat(64),"tasks":tasks
        })).unwrap()).unwrap();
        Self { root, project }
    }
    fn reference(&self, adapter: &TerminalBenchAdapter) -> AttemptReference {
        AttemptReference {
            schema_version: 1,
            preparation_id: "abcd-1234".into(),
            workload: adapter.id.into(),
            task_id: "task-0".into(),
            dataset: DatasetIdentity {
                source: adapter.source.into(),
                revision: adapter.revision.into(),
                split: adapter.split.into(),
            },
            artifacts: self.root.join(".asb/attempts/abcd-1234"),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn official_adapter_selection_retention_and_missing_sdk_are_explicit() {
    for adapter in [
        &super::super::terminal_bench_1_0::ADAPTER,
        &super::super::terminal_bench_2_1::ADAPTER,
    ] {
        let fixture = Fixture::new(adapter);
        assert_eq!(
            adapter.process_lifetime(),
            ProcessLifetime::ThroughEvaluation
        );
        assert_eq!(adapter.id(), adapter.id);
        assert_eq!(adapter.source(), Some(adapter.archive));
        assert!(adapter.trial_ids().is_empty());
        let workloads = fixture.root.join("workloads");
        let trials = adapter.installed_trials(&workloads).unwrap();
        assert_eq!(trials.len(), adapter.expected_tasks);
        assert_eq!(trials[0].id, "task-0");
        assert_eq!(trials[0].description, "Task 0");
        assert!(
            adapter
                .prepare_source(&fixture.root, &fixture.root)
                .unwrap_err()
                .contains("not a source-copy smoke trial")
        );
        let mut reference = fixture.reference(adapter);
        reference.dataset.revision = "different".into();
        let recovery = RecoveryContext {
            project_root: &fixture.root,
            project_config: &fixture.project,
            reference: &reference,
        };
        assert!(
            adapter
                .grade_retained(&recovery)
                .unwrap_err()
                .to_string()
                .contains("differs from its installed grading catalog")
        );
        reference.dataset.revision = adapter.revision.into();
        let recovery = RecoveryContext {
            project_root: &fixture.root,
            project_config: &fixture.project,
            reference: &reference,
        };
        for error in [
            adapter.grade_retained(&recovery).unwrap_err(),
            adapter.inspect_grade(&recovery).unwrap_err(),
            adapter.recover(&recovery).unwrap_err(),
        ] {
            assert!(error.to_string().contains("official evaluator"), "{error}");
        }
        let outcome = RetainedOutcomeContext {
            recovery,
            summary: &json!({}),
        };
        assert!(
            adapter
                .verify_retained(&outcome)
                .unwrap_err()
                .to_string()
                .contains("official evaluator")
        );
        let inputs = adapter
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
                    .join("tools")
                    .join(adapter.engine.tool())
                    .join("venv/bin/python"),
                site: true
            })
        );
    }
}

#[cfg(unix)]
#[test]
fn preparation_checks_selection_runtime_and_allocation_before_any_task_launch() {
    use std::os::unix::fs::symlink;
    for adapter in [
        &super::super::terminal_bench_1_0::ADAPTER,
        &super::super::terminal_bench_2_1::ADAPTER,
    ] {
        let fixture = Fixture::new(adapter);
        let workloads = fixture.root.join("workloads");
        let workspace = fixture.root.join("workspace");
        let observer = |_: &AttemptReference| {
            Err("fixture refuses allocation before process launch".to_owned())
        };
        let mut context = PreparationContext {
            verify_initial_material: false,
            budgets: Default::default(),
            on_allocation: Some(&observer),
            workload: adapter.id,
            trial: None,
            trial_set: None,
            workload_directory: &workloads,
            workspace: &workspace,
            project_root: &fixture.root,
            project_config: &fixture.project,
        };
        let reason = |context: &PreparationContext<'_>| {
            adapter
                .prepare_task(context)
                .err()
                .expect("fixture must never launch a task")
        };
        assert!(reason(&context).contains("select an official installed"));
        context.trial = Some("unknown-task");
        assert!(reason(&context).contains("absent from the installed dataset"));
        context.trial = Some("task-0");
        fs::create_dir_all(fixture.root.join("tools").join(adapter.engine.tool())).unwrap();
        let error = reason(&context);
        assert!(
            error.contains("cannot inspect installed evaluator runtime"),
            "{error}"
        );
        let interpreter = fixture
            .root
            .join("tools")
            .join(adapter.engine.tool())
            .join("venv/bin/python");
        fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
        assert_eq!(
            crate::install::OperationContext::load(interpreter.parent().unwrap())
                .unwrap()
                .root(),
            fixture.root,
            "the real runtime probe must resolve this initialized private project"
        );
        symlink("/bin/false", &interpreter).unwrap();
        assert!(reason(&context).contains("installed evaluator runtime is incomplete"));
        fs::remove_file(&interpreter).unwrap();
        // A true-returning runtime probe is a protocol fixture, not a claim
        // that a real evaluator is available. The observer stops before spawn.
        symlink("/bin/true", &interpreter).unwrap();
        assert_eq!(
            reason(&context),
            "fixture refuses allocation before process launch"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            fs::read_dir(&fixture.root)
                .unwrap()
                .filter(|entry| {
                    let entry = entry.as_ref().unwrap();
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".asb-install-step-")
                        && entry.path().join("output.log").is_file()
                })
                .count(),
            2,
            "both evaluator diagnostics stay in this adapter's private project"
        );
        assert!(
            !fixture
                .root
                .join(".asb/attempts")
                .read_dir()
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "log"))
        );
        assert_eq!(
            workspace.join(".asb-attempts").read_dir().unwrap().count(),
            1
        );
    }
}
