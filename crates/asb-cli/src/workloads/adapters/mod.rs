//! Adapters for externally sourced benchmark suites.
//!
//! Each supported suite has a module so suite-specific setup, trial discovery,
//! or result collection can be added without putting a branch in the runner.

use super::{
    KnownEvaluationKind, Trial, populate_builtin_fixtures, populate_builtin_prompts,
    prepare_builtin_fixture, prepare_external_source,
};
use std::path::Path;

/// Adapter-owned setup extension metadata.
///
/// The generic setup frontend owns navigation and publication; an adapter owns
/// the selectable domain and resolves its reserved choices.  `all` and `none`
/// are presentation keys, never persisted task identifiers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SetupExtension {
    pub id: &'static str,
    pub tab: &'static str,
    pub description: &'static str,
}

impl SetupExtension {
    pub(crate) const fn trials() -> Self {
        Self {
            id: "trial",
            tab: "trials",
            description: "Choose workload-specific trials. [all] toggles every available trial.",
        }
    }
}

mod agentbench;
mod agentdojo;
mod aider_polyglot;
mod bigcodebench;
mod core_bench;
mod evalplus;
mod livecodebench;
mod official_engine;
pub(super) mod prompt;
mod source_catalog;
mod swe_bench;
mod swe_bench_lite;
mod swe_bench_pro;
mod swe_bench_verified;
mod swe_efficiency;
mod swe_lancer;
mod swe_perf;
mod swe_rebench;
mod tau_bench;
mod terminal_bench;
mod terminal_bench_1_0;
mod terminal_bench_2_0;
mod terminal_bench_2_1;

pub(super) use source_catalog::SourceCatalogAdapter;

/// An adapter-owned source bundle installed by the generic workload installer.
#[derive(Clone, Copy)]
pub(crate) struct SourceBundle {
    pub archive: &'static str,
    pub marker: &'static str,
    pub prompt: &'static str,
}

pub(super) trait WorkloadAdapter: Sync {
    /// Conservative per-attempt workspace storage estimate.
    fn estimated_disk_mib(&self) -> u64 {
        1024
    }
    /// Optional setup frontend contribution. The frontend exposes this pane
    /// only while this adapter's workload remains selected, and discards its
    /// selection immediately when it is removed.
    fn setup_extension(&self) -> Option<SetupExtension> {
        None
    }
    /// Enumerate stable values for an adapter-owned setup extension. This is
    /// intentionally separate from presentation: adapters may use a catalog,
    /// a pinned predicate, or another bounded selector without teaching the
    /// CLI or TUI about their data format.
    fn setup_extension_values(
        &self,
        extension: &SetupExtension,
        directory: &Path,
    ) -> Result<Vec<String>, String> {
        if extension.id == "trial" {
            return self
                .installed_trials(directory)
                .map(|trials| trials.into_iter().map(|trial| trial.id).collect());
        }
        Err(format!("unsupported setup extension `{}`", extension.id))
    }
    fn descriptor(&self, name: &str) -> super::contracts::ModuleDescriptor {
        super::contracts::ModuleDescriptor::local(name, self.evaluation_kind(name))
    }
    fn process_lifetime(&self) -> super::session::ProcessLifetime {
        super::session::ProcessLifetime::Quiesce
    }
    fn grade_retained(
        &self,
        _context: &super::recovery::RecoveryContext<'_>,
    ) -> Result<serde_json::Value, crate::Error> {
        Err(crate::Error::Input("this workload cannot yet grade a retained unfinished submission; preserved work was not changed".into()))
    }
    fn inspect_grade(
        &self,
        _context: &super::recovery::RecoveryContext<'_>,
    ) -> Result<serde_json::Value, crate::Error> {
        Err(crate::Error::Input(
            "this workload cannot inspect retained official grades".into(),
        ))
    }
    fn verify_retained(
        &self,
        _context: &super::recovery::RetainedOutcomeContext<'_>,
    ) -> Result<(), crate::Error> {
        Err(crate::Error::Input(
            "this workload cannot validate retained upstream outcomes".into(),
        ))
    }
    fn id(&self) -> &'static str;
    fn recover(
        &self,
        _context: &super::recovery::RecoveryContext<'_>,
    ) -> Result<serde_json::Value, crate::Error> {
        Err(crate::Error::Input(
            "this workload adapter does not support recovery; preserved results were not changed"
                .into(),
        ))
    }
    /// Publish/download owned execution dependencies before creating attempts
    /// or taking an optional input snapshot. Local workloads do nothing.
    fn preflight(&self, _context: &super::PreflightContext<'_>) -> Result<(), crate::Error> {
        Ok(())
    }
    fn runtime_inputs(
        &self,
        _context: &crate::runtime_inputs::Context<'_>,
    ) -> Result<Vec<crate::runtime_inputs::Dependency>, crate::Error> {
        Ok(Vec::new())
    }
    fn source(&self) -> Option<&'static str> {
        None
    }
    fn source_bundle(&self) -> Option<SourceBundle> {
        None
    }
    fn install(
        &self,
        _context: &super::InstallationContext<'_>,
        _output: &mut dyn std::io::Write,
        _progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
    ) -> crate::install::InstallResult<bool> {
        Ok(false)
    }
    /// Complete an installation performed by the generic pinned-source
    /// installer.  Catalog-backed source bundles use this hook to derive their
    /// selectable task population before the default trial-set is written.
    fn finalize_install(&self, _directory: &Path) -> Result<(), String> {
        Ok(())
    }
    fn installed_available(&self, _directory: &Path) -> Option<bool> {
        None
    }
    fn installed_trials(&self, _directory: &Path) -> Result<Vec<Trial>, String> {
        Ok(self.trial_ids())
    }
    fn planning_catalog(
        &self,
        _directory: &Path,
    ) -> Result<Option<super::PlanningCatalog>, String> {
        Ok(None)
    }
    /// Smoke fixtures are not instances from the named upstream benchmark.
    fn evaluation_kind(&self, _name: &str) -> KnownEvaluationKind {
        KnownEvaluationKind::AdapterSmoke
    }
    fn evaluator(&self) -> super::session::Evaluator {
        super::evaluate_external_smoke
    }
    fn prepare_task(
        &self,
        context: &super::PreparationContext<'_>,
    ) -> Result<Box<dyn super::WorkloadSession>, String> {
        let prompt = super::load_trial(
            context.workload,
            context.trial,
            context.trial_set,
            context.workload_directory,
        )?;
        self.prepare_fixture(context.workload, context.workspace)?;
        self.prepare_source(context.workload_directory, context.workspace)?;
        Ok(Box::new(super::session::FileSession::new(
            context,
            prompt,
            self.evaluator(),
            self.evaluation_kind(context.workload),
        )))
    }
    fn matches(&self, name: &str) -> bool {
        self.id() == name
    }
    fn trial_ids(&self) -> Vec<Trial>;
    fn populate_prompts(&self, _directory: &Path) -> Result<usize, std::io::Error> {
        Ok(0)
    }
    fn populate_fixtures(&self, _directory: &Path) -> Result<usize, std::io::Error> {
        Ok(0)
    }
    fn prepare_fixture(&self, _name: &str, _workspace: &Path) -> Result<usize, String> {
        Ok(0)
    }
    fn prepare_source(&self, workloads: &Path, workspace: &Path) -> Result<usize, String>;
}

pub(super) struct PromptAdapter;
impl WorkloadAdapter for PromptAdapter {
    fn descriptor(&self, name: &str) -> super::contracts::ModuleDescriptor {
        let mut descriptor =
            super::contracts::ModuleDescriptor::local(name, self.evaluation_kind(name));
        if self.evaluation_kind(name) == KnownEvaluationKind::ResponseReview {
            descriptor.protocol.submission = super::contracts::SubmissionKind::ResponseText;
        }
        descriptor
    }
    fn evaluation_kind(&self, name: &str) -> KnownEvaluationKind {
        if name == "repository-navigation" {
            KnownEvaluationKind::ResponseReview
        } else {
            KnownEvaluationKind::BundledFixture
        }
    }
    fn evaluator(&self) -> super::session::Evaluator {
        prompt::evaluation::evaluate_builtin
    }
    fn id(&self) -> &'static str {
        "prompt"
    }
    fn matches(&self, name: &str) -> bool {
        super::is_builtin_prompt(name)
    }
    fn trial_ids(&self) -> Vec<Trial> {
        default_trials()
    }
    fn populate_prompts(&self, directory: &Path) -> Result<usize, std::io::Error> {
        populate_builtin_prompts(directory)
    }
    fn populate_fixtures(&self, directory: &Path) -> Result<usize, std::io::Error> {
        populate_builtin_fixtures(directory)
    }
    fn prepare_fixture(&self, name: &str, workspace: &Path) -> Result<usize, String> {
        prepare_builtin_fixture(name, workspace)
    }
    fn prepare_source(&self, _workloads: &Path, _workspace: &Path) -> Result<usize, String> {
        Ok(0)
    }
}

fn default_trials() -> Vec<Trial> {
    [
        ("smoke", "Small local smoke trial."),
        ("standard", "Standard workload trial."),
        ("extended", "Extended workload trial."),
    ]
    .into_iter()
    .map(|(id, description)| Trial {
        id: id.into(),
        description: description.into(),
    })
    .collect()
}

static REGISTRY: &[&dyn WorkloadAdapter] = &[
    &prompt::ADAPTER,
    &agentbench::ADAPTER,
    &agentdojo::ADAPTER,
    &aider_polyglot::ADAPTER,
    &bigcodebench::ADAPTER,
    &core_bench::ADAPTER,
    &evalplus::ADAPTER,
    &livecodebench::CODE_GENERATION_V1,
    &livecodebench::CODE_GENERATION_V2,
    &livecodebench::CODE_GENERATION_V3,
    &livecodebench::CODE_GENERATION_V4,
    &livecodebench::CODE_GENERATION_V5,
    &livecodebench::CODE_GENERATION_V6,
    &livecodebench::SELF_REPAIR_V1,
    &livecodebench::SELF_REPAIR_V2,
    &livecodebench::SELF_REPAIR_V3,
    &livecodebench::SELF_REPAIR_V4,
    &livecodebench::SELF_REPAIR_V5,
    &livecodebench::SELF_REPAIR_V6,
    &livecodebench::LATEST,
    &livecodebench::TEST_OUTPUT_PREDICTION,
    &livecodebench::CODE_EXECUTION,
    &swe_bench::ADAPTER,
    &swe_bench_lite::ADAPTER,
    &swe_bench_pro::ADAPTER,
    &swe_bench_verified::ADAPTER,
    &swe_efficiency::ADAPTER,
    &swe_lancer::ADAPTER,
    &swe_perf::ADAPTER,
    &swe_rebench::ADAPTER,
    &tau_bench::ADAPTER,
    &terminal_bench_1_0::ADAPTER,
    &terminal_bench_2_0::ADAPTER,
    &terminal_bench_2_1::ADAPTER,
];

pub(super) fn find(name: &str) -> Option<&'static dyn WorkloadAdapter> {
    REGISTRY
        .iter()
        .copied()
        .find(|adapter| adapter.matches(name))
}

pub(super) fn sources() -> impl Iterator<Item = (&'static str, &'static str)> {
    REGISTRY
        .iter()
        .filter_map(|adapter| adapter.source().map(|source| (adapter.id(), source)))
}

pub(super) fn populate_prompts(directory: &Path) -> Result<usize, std::io::Error> {
    REGISTRY.iter().try_fold(0, |count, adapter| {
        Ok(count + adapter.populate_prompts(directory)?)
    })
}

pub(super) fn populate_fixtures(directory: &Path) -> Result<usize, std::io::Error> {
    REGISTRY.iter().try_fold(0, |count, adapter| {
        Ok(count + adapter.populate_fixtures(directory)?)
    })
}

#[cfg(test)]
pub(super) fn names() -> impl Iterator<Item = &'static str> {
    REGISTRY
        .iter()
        .map(|adapter| adapter.id())
        .filter(|id| *id != "prompt")
}
