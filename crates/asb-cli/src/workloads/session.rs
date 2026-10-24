//! Adapter-owned state for one workload attempt.
//!
//! The runner consumes this contract, not dataset formats, test commands or
//! benchmark names. Upstream adapters can own containers and evaluator state
//! for exactly one attempt instead of sharing mutable state through a registry.

use super::Evaluation;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(0);

/// Scope of adapter-owned canonical initial material, excluding attempt paths.
pub(crate) const REPLAY_MATERIAL_CONTRACT: &str = "asb-initial-workload-material-v1";

/// Confirmed disposition of adapter-owned execution resources, not a grade.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceDisposition {
    /// No adapter-owned execution resources required settlement.
    NotRequired,
    /// Owned execution resources were removed.
    Removed,
    /// No owned execution resources were created.
    NotCreated,
    /// Owned execution resources are stopped and retained for inspection.
    StoppedPreserved,
}

impl ResourceDisposition {
    /// Legacy records may omit disposition; present values must agree exactly.
    pub(crate) fn matches_record(
        disposition: Option<&serde_json::Value>,
        confirmed: bool,
        summary: &serde_json::Value,
    ) -> bool {
        match (disposition, summary.get("workload_resource_disposition")) {
            (None, None) => true,
            (Some(top), Some(value)) if top == value => {
                serde_json::from_value::<Option<Self>>(value.clone())
                    .is_ok_and(|recorded| confirmed == recorded.is_some())
            }
            _ => false,
        }
    }
}

/// Positive settlement may retain a failed normal-teardown diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResourceSettlement {
    pub disposition: ResourceDisposition,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ProcessLifetime {
    #[default]
    Quiesce,
    ThroughEvaluation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DatasetIdentity {
    pub source: String,
    pub revision: String,
    pub split: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskIdentity {
    pub workload: String,
    pub task_id: String,
    pub attempt_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset: Option<DatasetIdentity>,
}

impl TaskIdentity {
    pub(crate) fn local(workload: &str, task_id: Option<&str>) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            workload: workload.into(),
            task_id: task_id.unwrap_or(workload).into(),
            attempt_id: format!(
                "{timestamp:x}-{:x}-{:x}",
                std::process::id(),
                NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed)
            ),
            dataset: None,
        }
    }
}

pub(crate) struct PreparationContext<'a> {
    pub verify_initial_material: bool,
    pub budgets: crate::planning::PhaseBudgets,
    pub on_allocation: Option<&'a super::recovery::AllocationObserver<'a>>,
    pub workload: &'a str,
    pub trial: Option<&'a str>,
    pub trial_set: Option<&'a str>,
    pub workload_directory: &'a Path,
    pub workspace: &'a Path,
    pub project_root: &'a Path,
    pub project_config: &'a crate::project::ProjectConfig,
}

impl PreparationContext<'_> {
    pub(crate) fn tool_root(&self, id: &str) -> PathBuf {
        crate::project::resolve(self.project_root, &self.project_config.tools_dir).join(id)
    }
}

pub(crate) struct TaskInvocation {
    pub identity: TaskIdentity,
    pub input: TaskInput,
    pub workspace: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskInput {
    pub instructions: String,
    pub attachments: Vec<TaskAttachment>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskAttachment {
    /// Relative artifact reference under the task workspace, never an inline
    /// payload or unbounded controller message.
    pub artifact: PathBuf,
    pub media_type: String,
    pub sha256: String,
    pub byte_length: u64,
}

impl From<String> for TaskInput {
    fn from(instructions: String) -> Self {
        Self {
            instructions,
            attachments: Vec::new(),
        }
    }
}
impl From<&str> for TaskInput {
    fn from(instructions: &str) -> Self {
        instructions.to_owned().into()
    }
}
impl TaskInput {
    pub(crate) fn validate_for_agent(&self, agent: &str) -> Result<(), String> {
        if self.instructions.is_empty()
            || self.instructions.len() > 64 * 1024
            || self.attachments.len() > 32
        {
            return Err("task input exceeds invocation bounds".into());
        }
        let supported =
            crate::agents::find(agent).map_or(&["text"][..], |adapter| adapter.input_modalities());
        if !supported.contains(&"text") {
            return Err("agent does not support task instructions".into());
        }
        for attachment in &self.attachments {
            let modality = attachment
                .media_type
                .split_once('/')
                .map(|(kind, _)| kind)
                .ok_or("invalid attachment media type")?;
            if attachment.artifact.as_os_str().is_empty()
                || attachment.artifact.as_os_str().len() > 4096
                || attachment.artifact.is_absolute()
                || attachment
                    .artifact
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
                || attachment.byte_length == 0
                || attachment.byte_length > 256 * 1024 * 1024
                || attachment.sha256.len() != 64
                || !attachment
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || attachment.media_type.len() > 128
                || attachment.media_type.chars().any(char::is_control)
            {
                return Err("invalid bounded task attachment reference".into());
            }
            if !supported.contains(&modality) {
                return Err(format!(
                    "agent input transport does not support {modality} attachments"
                ));
            }
        }
        if !self.attachments.is_empty() {
            return Err("agent input transport does not deliver task attachments yet; inline supported text in instructions instead".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;
    #[test]
    fn structured_inputs_refuse_unsupported_modalities_and_invalid_artifact_references() {
        let mut input: TaskInput = "solve the fixture".into();
        assert!(input.validate_for_agent("pi").is_ok());
        input.attachments.push(TaskAttachment {
            artifact: "image.png".into(),
            media_type: "image/png".into(),
            sha256: "a".repeat(64),
            byte_length: 1024,
        });
        assert!(
            input
                .validate_for_agent("pi")
                .unwrap_err()
                .contains("does not support image")
        );
        input.attachments[0].artifact = "../private/image.png".into();
        assert!(
            input
                .validate_for_agent("pi")
                .unwrap_err()
                .contains("invalid bounded")
        );
        input.attachments.clear();
        input.attachments.push(TaskAttachment {
            artifact: "missing-unverified.txt".into(),
            media_type: "text/plain".into(),
            sha256: "a".repeat(64),
            byte_length: 1024,
        });
        assert!(
            input
                .validate_for_agent("pi")
                .unwrap_err()
                .contains("does not deliver task attachments")
        );
        input.attachments.clear();
        input.instructions = "x".repeat(64 * 1024 + 1);
        assert!(input.validate_for_agent("pi").is_err());
    }
}

/// The runner provides negotiated connections, never workload/agent dispatch.
pub(crate) trait LaunchReadiness {
    fn ready(&self) -> Result<(), String>;
    fn abort(&self);
    fn remaining(&self) -> Result<std::time::Duration, String>;
}

/// Declare after owned execution state so peers are notified before teardown.
pub(crate) struct LaunchFailureGuard<'a>(pub Option<&'a dyn LaunchReadiness>);
impl Drop for LaunchFailureGuard<'_> {
    fn drop(&mut self) {
        if let Some(launch) = self.0 {
            launch.abort();
        }
    }
}

/// The runner provides negotiated connections, never workload/agent dispatch.
pub(crate) struct ExecutionContext<'a> {
    pub launch: Option<&'a dyn LaunchReadiness>,
    pub config: &'a crate::benchmark::BenchmarkConfig,
    pub connection: Option<&'a crate::endpoint::Connection>,
    pub project_root: &'a Path,
    pub project_config: &'a crate::project::ProjectConfig,
    pub replay: bool,
    /// Exact provider cassette identity when this execution is tape-backed.
    /// This is forwarded to guest agent preparation so adapter-owned tool
    /// replay never guesses from the loopback endpoint.
    pub tape_cassette: Option<&'a Path>,
    pub measurements: Option<&'a mut crate::measurements::Session>,
    pub show_output: bool,
    /// Presentation is best-effort and deliberately cannot influence workload
    /// ownership, transport, or result custody.
    pub progress: Option<&'a mut dyn ExecutionProgress>,
}

/// A live, presentation-only view of an agent transcript. Both native child
/// capture and isolated workload transports use this same narrow contract.
pub(crate) trait ExecutionProgress {
    fn update(&mut self, stdout: &[u8], stderr: &[u8]);
    fn finish(&mut self);
}

pub(crate) struct ExecutionResult {
    pub output: std::process::Output,
    pub metadata: crate::agents::driver::GuestResult,
    pub elapsed_ms: Option<u128>,
    /// Post-execution instrumentation cannot invalidate an available submission.
    pub collector_errors: Vec<String>,
    pub output_error: Option<String>,
}

/// Baseline provenance is not a boolean: an official clean task definition
/// does not claim that ASB ran its initial failing tests.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum BaselineEvidence {
    Measured {
        passed: Option<bool>,
    },
    UpstreamTaskDefinition {
        base_commit: String,
    },
    UpstreamEnvironmentDefinition {
        source_revision: String,
        definition_sha256: String,
    },
    Unavailable,
}

impl BaselineEvidence {
    pub(crate) fn measured_passed(&self) -> Option<bool> {
        match self {
            Self::Measured { passed } => *passed,
            _ => None,
        }
    }

    pub(crate) fn supports(&self, evaluation_kind: &str) -> bool {
        self.supports_evaluation(super::SavedEvaluationKind::classify(evaluation_kind))
    }

    pub(crate) fn supports_evaluation(
        &self,
        evaluation_kind: super::SavedEvaluationKind<'_>,
    ) -> bool {
        let hash = |value: &str, length| {
            value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        };
        match self {
            Self::Measured {
                passed: Some(false),
            } => true,
            Self::UpstreamTaskDefinition { base_commit } => {
                evaluation_kind
                    == super::SavedEvaluationKind::Known(
                        super::KnownEvaluationKind::UpstreamHarness,
                    )
                    && base_commit.len() == 40
                    && base_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            }
            Self::UpstreamEnvironmentDefinition {
                source_revision,
                definition_sha256,
            } => {
                evaluation_kind
                    == super::SavedEvaluationKind::Known(
                        super::KnownEvaluationKind::UpstreamHarness,
                    )
                    && hash(source_revision, 40)
                    && hash(definition_sha256, 64)
            }
            _ => false,
        }
    }

    /// Legacy records retain their original measured-baseline semantics.
    /// An explicitly malformed new field must never fall back to a legacy flag.
    pub(crate) fn from_record(record: &serde_json::Value) -> Self {
        match record.get("workload_baseline") {
            Some(value) => serde_json::from_value(value.clone()).unwrap_or(Self::Unavailable),
            None => Self::Measured {
                passed: record
                    .get("workload_baseline_passed")
                    .and_then(serde_json::Value::as_bool),
            },
        }
    }
}

pub(crate) trait WorkloadSession {
    /// Effective controller caps, not a claim about elapsed time or a hidden
    /// upstream verifier's own (possibly tighter) task deadline.
    fn effective_budgets(&self) -> crate::planning::EffectiveBudgets {
        crate::planning::EffectiveBudgets::default()
    }
    fn invocation(&self) -> &TaskInvocation;
    fn evaluation_kind(&self) -> super::KnownEvaluationKind;
    fn baseline_passed(&self) -> Option<bool>;
    /// Adapter-owned initial material, captured before baseline evaluation.
    /// Unsupported identity must not be replaced with an invented fingerprint.
    fn initial_material_sha256(&self) -> Result<String, String> {
        Err("workload adapter does not yet establish initial material identity".into())
    }
    /// Abort preparation without authorizing normal destructive teardown.
    fn cancel_prepared(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn baseline_evidence(&self) -> BaselineEvidence {
        BaselineEvidence::Measured {
            passed: self.baseline_passed(),
        }
    }
    fn evaluate(&mut self, final_response: &str) -> Result<Evaluation, String>;
    /// Modules may override this for asynchronous or non-binary evaluators.
    fn evaluate_outcome(&mut self, final_response: &str) -> super::outcome::EvaluationOutcome {
        match self.evaluate(final_response) {
            Ok(Evaluation::Passed) => super::outcome::EvaluationOutcome::binary(true),
            Ok(Evaluation::Failed(reason)) => {
                let mut outcome = super::outcome::EvaluationOutcome::binary(false);
                if let super::outcome::EvaluationOutcome::Completed {
                    reason: diagnostic, ..
                } = &mut outcome
                {
                    *diagnostic = Some(reason);
                }
                outcome
            }
            Err(reason) => super::outcome::EvaluationOutcome::Error { reason },
        }
    }
    fn metric_policy(&self) -> super::outcome::MetricPolicy {
        super::outcome::MetricPolicy::default()
    }
    fn module_descriptor(&self) -> Option<super::contracts::ModuleDescriptor> {
        super::descriptor(&self.invocation().identity.workload)
    }
    /// Adapter-owned evaluator artifacts that bind a historical task outcome.
    /// This is not a request to inspect unrelated project or host directories.
    fn checkpoint_evidence(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    /// Settle execution resources independently of grading. Err means unconfirmed.
    fn finish(&mut self) -> Result<ResourceSettlement, String> {
        Ok(ResourceSettlement {
            disposition: ResourceDisposition::NotRequired,
            diagnostic: None,
        })
    }
    fn execute(
        &mut self,
        _context: &mut ExecutionContext<'_>,
        _output: &mut dyn std::io::Write,
    ) -> Option<Result<ExecutionResult, String>> {
        None
    }
}

pub(super) type Evaluator = fn(&str, &Path, &str) -> Result<Evaluation, String>;

pub(super) struct FileSession {
    invocation: TaskInvocation,
    evaluator: Evaluator,
    evaluation_kind: super::KnownEvaluationKind,
    baseline_passed: Option<bool>,
    initial_material: Result<String, String>,
}

impl FileSession {
    pub fn new(
        context: &PreparationContext<'_>,
        prompt: String,
        evaluator: Evaluator,
        evaluation_kind: super::KnownEvaluationKind,
    ) -> Self {
        let initial_material = if context.verify_initial_material {
            super::material::fingerprint(context.workspace)
        } else {
            Err("initial material identity was not requested for this legacy execution".into())
        };
        let baseline_passed = evaluator(context.workload, context.workspace, "")
            .ok()
            .map(|result| result == Evaluation::Passed);
        let initial_material = initial_material.and_then(|before| {
            if super::material::fingerprint(context.workspace)? == before {
                Ok(before)
            } else {
                Err("baseline evaluation changed initial workload material".into())
            }
        });
        Self {
            invocation: TaskInvocation {
                identity: TaskIdentity::local(context.workload, context.trial),
                input: prompt.into(),
                workspace: context.workspace.to_path_buf(),
            },
            evaluator,
            evaluation_kind,
            baseline_passed,
            initial_material,
        }
    }
}

impl WorkloadSession for FileSession {
    fn initial_material_sha256(&self) -> Result<String, String> {
        self.initial_material.clone()
    }
    fn invocation(&self) -> &TaskInvocation {
        &self.invocation
    }
    fn evaluation_kind(&self) -> super::KnownEvaluationKind {
        self.evaluation_kind
    }
    fn baseline_passed(&self) -> Option<bool> {
        self.baseline_passed
    }
    fn evaluate(&mut self, final_response: &str) -> Result<Evaluation, String> {
        (self.evaluator)(
            &self.invocation.identity.workload,
            &self.invocation.workspace,
            final_response,
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn recorded_settlement_is_strict_without_inventing_legacy_removal() {
        use super::ResourceDisposition as D;
        use serde_json::json;
        let fields = [
            None,
            Some(json!(null)),
            Some(json!("removed")),
            Some(json!("stopped-preserved")),
            Some(json!("unknown")),
        ];
        for top in &fields {
            for nested in &fields {
                for confirmed in [false, true] {
                    let mut summary = json!({});
                    if let Some(value) = nested {
                        summary["workload_resource_disposition"] = value.clone();
                    }
                    let expected = match (top, nested) {
                        (None, None) => true,
                        (Some(a), Some(b)) if a == b => {
                            (a.is_null() && !confirmed)
                                || ((a == "removed" || a == "stopped-preserved") && confirmed)
                        }
                        _ => false,
                    };
                    assert_eq!(
                        D::matches_record(top.as_ref(), confirmed, &summary),
                        expected,
                        "top={top:?}, summary={nested:?}, confirmed={confirmed}"
                    );
                }
            }
        }
        for disposition in [
            D::NotRequired,
            D::Removed,
            D::NotCreated,
            D::StoppedPreserved,
        ] {
            let record = json!({"workload_resource_disposition":disposition});
            assert!(D::matches_record(
                record.get("workload_resource_disposition"),
                true,
                &record
            ));
            assert!(!D::matches_record(
                record.get("workload_resource_disposition"),
                false,
                &record
            ));
            assert!(!D::matches_record(None, true, &record));
        }
        assert!(!D::matches_record(
            None,
            true,
            &json!({"workload_resource_disposition":"unknown"})
        ));
    }

    #[test]
    fn evaluator_side_effects_cannot_become_the_verified_initial_material() {
        use super::*;
        fn changes_files(
            _: &str,
            workspace: &std::path::Path,
            _: &str,
        ) -> Result<super::Evaluation, String> {
            std::fs::create_dir(workspace.join("__pycache__")).unwrap();
            std::fs::write(
                workspace.join("__pycache__/fixture.pyc"),
                "evaluator side effect",
            )
            .unwrap();
            Ok(super::Evaluation::Failed(
                "expected baseline failure".into(),
            ))
        }
        let root = std::env::temp_dir()
            .join(TaskIdentity::local("material-before-evaluator", None).attempt_id);
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("fixture.py"), "original material").unwrap();
        let project = crate::project::ProjectConfig::default();
        let context = PreparationContext {
            verify_initial_material: true,
            budgets: Default::default(),
            on_allocation: None,
            workload: "fixture",
            trial: None,
            trial_set: None,
            workload_directory: &root,
            workspace: &root,
            project_root: &root,
            project_config: &project,
        };
        let session = FileSession::new(
            &context,
            "prompt".into(),
            changes_files,
            super::super::KnownEvaluationKind::BundledFixture,
        );
        assert_eq!(session.baseline_passed(), Some(false));
        assert_eq!(
            session.initial_material_sha256().unwrap_err(),
            "baseline evaluation changed initial workload material"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("fixture.py")).unwrap(),
            "original material"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn external_catalog_location_cannot_select_another_projects_tools() {
        let config = crate::project::ProjectConfig {
            tools_dir: "local-tools".into(),
            ..Default::default()
        };
        let context = super::PreparationContext {
            verify_initial_material: false,
            budgets: Default::default(),
            on_allocation: None,
            workload: "fixture",
            trial: None,
            trial_set: None,
            workload_directory: std::path::Path::new("/shared/catalogs/outside-the-project"),
            workspace: std::path::Path::new("/fixture/project/workspace"),
            project_root: std::path::Path::new("/fixture/project"),
            project_config: &config,
        };
        assert_eq!(
            context.tool_root("fixture-sdk"),
            std::path::Path::new("/fixture/project/local-tools/fixture-sdk")
        );
    }
    use super::*;

    #[test]
    fn official_definition_is_not_a_measured_failing_baseline() {
        let baseline = BaselineEvidence::UpstreamTaskDefinition {
            base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        };
        assert_eq!(baseline.measured_passed(), None);
        assert!(baseline.supports("upstream-harness"));
        assert!(!baseline.supports("bundled-fixture"));
        assert_eq!(
            super::super::scored_kind("upstream-harness", &baseline),
            "upstream-harness"
        );
        assert_eq!(
            super::super::scored_kind("bundled-fixture", &baseline),
            "invalid-baseline"
        );
        let record = serde_json::json!({"workload_baseline": baseline});
        assert_eq!(BaselineEvidence::from_record(&record), baseline);
    }

    #[test]
    fn baseline_provenance_rejects_malformed_and_explicitly_missing_evidence() {
        for value in [
            serde_json::json!(null),
            serde_json::json!({"kind":"unavailable"}),
            serde_json::json!({"kind":"upstream-task-definition","base_commit":"HEAD"}),
            serde_json::json!({"kind":"upstream-task-definition","base_commit":"g".repeat(40)}),
            serde_json::json!({"kind":"upstream-task-definition","base_commit":"a".repeat(40),"unexpected":true}),
            serde_json::json!({"kind":"measured","passed":true}),
        ] {
            let record =
                serde_json::json!({"workload_baseline":value,"workload_baseline_passed":false});
            let evidence = BaselineEvidence::from_record(&record);
            assert!(!evidence.supports("upstream-harness"));
        }
        let legacy = serde_json::json!({"workload_baseline_passed":false});
        assert!(BaselineEvidence::from_record(&legacy).supports("bundled-fixture"));
        assert!(!BaselineEvidence::from_record(&serde_json::json!({})).supports("bundled-fixture"));
    }

    #[test]
    fn environment_definition_does_not_invent_a_repository_or_measured_test_baseline() {
        let baseline = BaselineEvidence::UpstreamEnvironmentDefinition {
            source_revision: "a".repeat(40),
            definition_sha256: "b".repeat(64),
        };
        assert!(baseline.supports("upstream-harness"));
        assert!(!baseline.supports("bundled-fixture"));
        assert_eq!(baseline.measured_passed(), None);
        assert_eq!(
            BaselineEvidence::from_record(&serde_json::json!({"workload_baseline":baseline})),
            baseline
        );
        let bad = BaselineEvidence::UpstreamEnvironmentDefinition {
            source_revision: "HEAD".into(),
            definition_sha256: "b".repeat(64),
        };
        assert!(!bad.supports("upstream-harness"));
    }

    #[test]
    fn repeated_tasks_have_distinct_attempts_without_changing_task_identity() {
        let first = TaskIdentity::local("suite", Some("owner__repo-123"));
        let second = TaskIdentity::local("suite", Some("owner__repo-123"));
        assert_eq!(first.workload, second.workload);
        assert_eq!(first.task_id, second.task_id);
        assert_ne!(first.attempt_id, second.attempt_id);
        let encoded = serde_json::to_string(&first).unwrap();
        assert_eq!(
            serde_json::from_str::<TaskIdentity>(&encoded).unwrap(),
            first
        );
        assert!(!encoded.contains("dataset"));
    }

    #[test]
    fn dataset_revision_and_split_survive_serialization() {
        let mut task = TaskIdentity::local("suite", Some("case-1"));
        task.dataset = Some(DatasetIdentity {
            source: "official/dataset".into(),
            revision: "pinned-revision".into(),
            split: "test".into(),
        });
        assert_eq!(
            serde_json::from_value::<TaskIdentity>(serde_json::to_value(&task).unwrap()).unwrap(),
            task
        );
    }

    #[test]
    fn session_owns_prepared_prompt_workspace_and_evaluator() {
        let root = std::env::temp_dir().join(format!(
            "asb-session-{}",
            TaskIdentity::local("fixture", None).attempt_id
        ));
        let workloads = root.join("workloads");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workloads).unwrap();
        super::super::populate(&workloads).unwrap();
        let mut session = super::super::prepare_task(&PreparationContext {
            verify_initial_material: true,
            budgets: Default::default(),
            on_allocation: None,
            workload: "bug-fix",
            trial: None,
            trial_set: None,
            workload_directory: &workloads,
            workspace: &workspace,
            project_root: &root,
            project_config: &crate::project::ProjectConfig::default(),
        })
        .unwrap();
        assert_eq!(session.invocation().workspace, workspace);
        assert_eq!(session.invocation().identity.task_id, "bug-fix");
        assert!(!session.invocation().input.instructions.trim().is_empty());
        assert_eq!(session.baseline_passed(), Some(false));
        let initial_material = session.initial_material_sha256().unwrap();
        assert_eq!(session.evaluation_kind().as_str(), "bundled-fixture");
        assert!(matches!(
            session.evaluate("done"),
            Ok(Evaluation::Failed(_))
        ));
        std::fs::write(workspace.join("src/parser.py"), "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n").unwrap();
        assert_eq!(session.evaluate(""), Ok(Evaluation::Passed));
        // Baseline evidence belongs to this attempt, not the mutated workspace.
        assert_eq!(session.baseline_passed(), Some(false));
        assert_eq!(session.initial_material_sha256().unwrap(), initial_material);
        let later = super::super::prepare_task(&PreparationContext {
            verify_initial_material: true,
            budgets: Default::default(),
            on_allocation: None,
            workload: "bug-fix",
            trial: None,
            trial_set: None,
            workload_directory: &workloads,
            workspace: &workspace,
            project_root: &root,
            project_config: &crate::project::ProjectConfig::default(),
        })
        .unwrap();
        assert_eq!(later.baseline_passed(), Some(true));
        assert_ne!(later.initial_material_sha256().unwrap(), initial_material);
        assert_ne!(
            later.invocation().identity.attempt_id,
            session.invocation().identity.attempt_id
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
