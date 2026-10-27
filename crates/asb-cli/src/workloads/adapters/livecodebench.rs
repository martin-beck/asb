//! Pinned LiveCodeBench task populations.
//!
//! The upstream project deliberately publishes multiple scenario-specific
//! datasets. This adapter owns their installation and selection metadata so
//! the generic CLI and TUI never infer dataset names or trials.
mod catalog;

use super::WorkloadAdapter;
use crate::workloads::session::{DatasetIdentity, FileSession, PreparationContext};
use crate::workloads::{Evaluation, KnownEvaluationKind};
use crate::workloads::{InstallationContext, PlanningCatalog, Trial, WorkloadSession};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

pub(super) struct LiveCodeBenchAdapter {
    pub id: &'static str,
    pub scenario: &'static str,
    pub dataset: &'static str,
    pub revision: &'static str,
    pub release: Option<&'static str>,
}

const CODE_GENERATION_DATASET: &str = "livecodebench/code_generation_lite";
const CODE_GENERATION_REVISION: &str = "0fe84c3912ea0c4d4a78037083943e8f0c4dd505";
const TEST_OUTPUT_DATASET: &str = "livecodebench/test_generation";
const TEST_OUTPUT_REVISION: &str = "6f3ac40bbecf81eba15899139d279b077f2816fd";
const CODE_EXECUTION_DATASET: &str = "livecodebench/execution-v2";
const CODE_EXECUTION_REVISION: &str = "ff6ea0e2a638001006ddcc41259eff23a4283fb2";

macro_rules! generation_adapters {
    ($($field:ident => ($id:literal, $scenario:literal, $release:expr)),+ $(,)?) => {
        $(pub(super) static $field: LiveCodeBenchAdapter = LiveCodeBenchAdapter {
            id: $id,
            scenario: $scenario,
            dataset: CODE_GENERATION_DATASET,
            revision: CODE_GENERATION_REVISION,
            release: Some($release),
        };)+
    };
}

generation_adapters!(
    CODE_GENERATION_V1 => ("livecodebench-code-generation-v1", "code-generation", "release_v1"),
    CODE_GENERATION_V2 => ("livecodebench-code-generation-v2", "code-generation", "release_v2"),
    CODE_GENERATION_V3 => ("livecodebench-code-generation-v3", "code-generation", "release_v3"),
    CODE_GENERATION_V4 => ("livecodebench-code-generation-v4", "code-generation", "release_v4"),
    CODE_GENERATION_V5 => ("livecodebench-code-generation-v5", "code-generation", "release_v5"),
    CODE_GENERATION_V6 => ("livecodebench-code-generation-v6", "code-generation", "release_v6"),
    SELF_REPAIR_V1 => ("livecodebench-self-repair-v1", "self-repair", "release_v1"),
    SELF_REPAIR_V2 => ("livecodebench-self-repair-v2", "self-repair", "release_v2"),
    SELF_REPAIR_V3 => ("livecodebench-self-repair-v3", "self-repair", "release_v3"),
    SELF_REPAIR_V4 => ("livecodebench-self-repair-v4", "self-repair", "release_v4"),
    SELF_REPAIR_V5 => ("livecodebench-self-repair-v5", "self-repair", "release_v5"),
    SELF_REPAIR_V6 => ("livecodebench-self-repair-v6", "self-repair", "release_v6"),
);

/// The short family name remains the current recommended code-generation
/// release. Explicit release IDs are available whenever reproducibility across
/// LiveCodeBench releases matters.
pub(super) static LATEST: LiveCodeBenchAdapter = LiveCodeBenchAdapter {
    id: "livecodebench",
    scenario: "code-generation",
    dataset: CODE_GENERATION_DATASET,
    revision: CODE_GENERATION_REVISION,
    release: Some("release_v6"),
};

pub(super) static TEST_OUTPUT_PREDICTION: LiveCodeBenchAdapter = LiveCodeBenchAdapter {
    id: "livecodebench-test-output-prediction",
    scenario: "test-output-prediction",
    dataset: TEST_OUTPUT_DATASET,
    revision: TEST_OUTPUT_REVISION,
    release: None,
};

pub(super) static CODE_EXECUTION: LiveCodeBenchAdapter = LiveCodeBenchAdapter {
    id: "livecodebench-code-execution",
    scenario: "code-execution",
    dataset: CODE_EXECUTION_DATASET,
    revision: CODE_EXECUTION_REVISION,
    release: None,
};

impl WorkloadAdapter for LiveCodeBenchAdapter {
    fn descriptor(&self, _name: &str) -> crate::workloads::contracts::ModuleDescriptor {
        use crate::workloads::contracts::{
            EnvironmentKind, EvaluationMode, ModuleDescriptor, SubmissionKind,
        };
        let mut descriptor = ModuleDescriptor::official(
            self.id,
            "livecodebench",
            self.release.unwrap_or(self.scenario),
            DatasetIdentity {
                source: self.dataset.into(),
                revision: self.revision.into(),
                split: self.release.unwrap_or(self.scenario).into(),
            },
            "livecodebench",
            SubmissionKind::ResponseText,
            EvaluationMode::Local,
        );
        descriptor.protocol.environment = EnvironmentKind::Native;
        descriptor
    }
    fn id(&self) -> &'static str {
        self.id
    }

    fn source(&self) -> Option<&'static str> {
        Some(
            "https://github.com/LiveCodeBench/LiveCodeBench/tree/28fef95ea8c9f7a547c8329f2cd3d32b92c1fa24",
        )
    }

    fn setup_extension(&self) -> Option<super::SetupExtension> {
        Some(super::SetupExtension::trials())
    }

    fn install(
        &self,
        context: &InstallationContext<'_>,
        output: &mut dyn std::io::Write,
        progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
    ) -> crate::install::InstallResult<bool> {
        catalog::install(self, context, output, progress)?;
        Ok(true)
    }

    fn installed_available(&self, directory: &Path) -> Option<bool> {
        Some(catalog::read(self, directory).is_ok())
    }

    fn installed_trials(&self, directory: &Path) -> Result<Vec<Trial>, String> {
        Ok(catalog::read(self, directory)?
            .tasks
            .into_iter()
            .map(|task| Trial {
                id: task.id,
                description: task.description,
            })
            .collect())
    }

    fn trial_ids(&self) -> Vec<Trial> {
        Vec::new()
    }

    fn planning_catalog(&self, directory: &Path) -> Result<Option<PlanningCatalog>, String> {
        let catalog = catalog::read(self, directory)?;
        Ok(Some(PlanningCatalog {
            requirements: catalog
                .tasks
                .iter()
                .map(|task| {
                    (
                        task.id.clone(),
                        crate::planning::TaskRequirements {
                            source: "livecodebench-official-evaluator".into(),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            dataset: catalog.dataset,
            task_ids: catalog.tasks.into_iter().map(|task| task.id).collect(),
            evaluation_kind: KnownEvaluationKind::UpstreamHarness,
        }))
    }

    fn prepare_task(
        &self,
        context: &PreparationContext<'_>,
    ) -> Result<Box<dyn WorkloadSession>, String> {
        let catalog_data = catalog::read(self, context.workload_directory)?;
        let task_id = context
            .trial
            .ok_or("select an installed LiveCodeBench instance before execution")?;
        let task = catalog_data
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .ok_or("selected instance is absent from the installed LiveCodeBench catalog")?;
        let tool = context.tool_root("livecodebench");
        let catalog_path = context
            .workload_directory
            .join(self.id)
            .join("catalog.json");
        let source = context.project_root.join(
            ".asb-cache/huggingface/livecodebench-source/28fef95ea8c9f7a547c8329f2cd3d32b92c1fa24",
        );
        let project_root = context.project_root.to_path_buf();
        let task_id = task.id.clone();
        Ok(Box::new(FileSession::new_dynamic(
            context,
            task.prompt.clone(),
            Box::new(move |_, _, response| {
                evaluate(
                    &tool,
                    &catalog_path,
                    &source,
                    &project_root,
                    &task_id,
                    response,
                )
            }),
            KnownEvaluationKind::UpstreamHarness,
            None,
        )))
    }

    fn prepare_source(&self, _workloads: &Path, _workspace: &Path) -> Result<usize, String> {
        Err("LiveCodeBench tasks require their installed official task catalog".into())
    }
}

fn evaluate(
    tool: &Path,
    catalog: &Path,
    source: &Path,
    project_root: &Path,
    task: &str,
    response: &str,
) -> Result<Evaluation, String> {
    const RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
    const RESULT_LIMIT: u64 = 64 * 1024;
    if response.len() > RESPONSE_LIMIT {
        return Err("LiveCodeBench response exceeds the 8 MiB evaluator input limit".into());
    }
    if !tool.join("venv/bin/python").is_file()
        || !tool.join("asb_livecodebench_catalog.py").is_file()
    {
        return Err(
            "LiveCodeBench official evaluator is unavailable; rerun `asb workload install`".into(),
        );
    }
    let root = project_root.join(".asb/livecodebench-evaluation");
    fs::create_dir_all(&root)
        .map_err(|_| "cannot prepare private LiveCodeBench evaluation input")?;
    let id =
        crate::workloads::TaskIdentity::local("livecodebench-evaluation", Some(task)).attempt_id;
    let response_path = root.join(format!("{id}.response"));
    let result_path = root.join(format!("{id}.json"));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&response_path)
            .map_err(|_| "cannot create private LiveCodeBench evaluation input")?;
        std::io::Write::write_all(&mut file, response.as_bytes())
            .map_err(|_| "cannot write private LiveCodeBench evaluation input")?;
        let status = Command::new(tool.join("venv/bin/python"))
            .arg(tool.join("asb_livecodebench_catalog.py"))
            .args(["evaluate", "--catalog"])
            .arg(catalog)
            .args(["--task", task, "--response"])
            .arg(&response_path)
            .args(["--source"])
            .arg(source)
            .args(["--result"])
            .arg(&result_path)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            // Evaluation consumes only the installed task catalog and the
            // checked, pinned evaluator source. A benchmark run must never
            // turn an evaluator import into a dataset or hub request.
            .env("HF_HUB_OFFLINE", "1")
            .env("HF_DATASETS_OFFLINE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| "cannot start the LiveCodeBench official evaluator")?;
        if !status.success() {
            return Err("LiveCodeBench official evaluator failed; inspect the bounded project installation log and preserve the task result".into());
        }
        let metadata = fs::metadata(&result_path)
            .map_err(|_| "LiveCodeBench evaluator did not publish a result")?;
        if !metadata.is_file() || metadata.len() > RESULT_LIMIT {
            return Err("LiveCodeBench evaluator result is invalid or exceeds 64 KiB".into());
        }
        let mut bytes = Vec::new();
        fs::File::open(&result_path)
            .map_err(|_| "cannot read LiveCodeBench evaluator result")?
            .take(RESULT_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read LiveCodeBench evaluator result")?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| "LiveCodeBench evaluator returned invalid JSON")?;
        match (value["schema_version"].as_u64(), value["passed"].as_bool()) {
            (Some(1), Some(true)) => Ok(Evaluation::Passed),
            (Some(1), Some(false)) => Ok(Evaluation::Failed(
                "official LiveCodeBench evaluator rejected the response".into(),
            )),
            _ => Err("LiveCodeBench evaluator result has an unsupported schema".into()),
        }
    })();
    let _ = fs::remove_file(&response_path);
    let _ = fs::remove_file(&result_path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_official_livecodebench_population_has_an_adapter_owned_trial_extension() {
        for adapter in [
            &CODE_GENERATION_V1,
            &CODE_GENERATION_V2,
            &CODE_GENERATION_V3,
            &CODE_GENERATION_V4,
            &CODE_GENERATION_V5,
            &CODE_GENERATION_V6,
            &SELF_REPAIR_V1,
            &SELF_REPAIR_V2,
            &SELF_REPAIR_V3,
            &SELF_REPAIR_V4,
            &SELF_REPAIR_V5,
            &SELF_REPAIR_V6,
            &TEST_OUTPUT_PREDICTION,
            &CODE_EXECUTION,
            &LATEST,
        ] {
            assert_eq!(
                adapter.setup_extension(),
                Some(super::super::SetupExtension::trials())
            );
        }
    }
}
