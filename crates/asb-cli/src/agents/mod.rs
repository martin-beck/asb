//! Extensible adapters for benchmark agents.
//!
//! The runner owns only process lifecycle and output capture. Every command
//! shape and provider-specific adjustment lives in the owning adapter module.

mod claude_code;
mod cline;
mod codex;
mod continue_agent;
mod goose;
mod mini_swe;
mod opencode;
mod openhands;
mod openjiuwen;
mod pi;
mod qwen_code;
mod workbuddy;

use crate::Error;
use crate::benchmark::BenchmarkConfig;
use crate::install::Recipe;
use crate::project;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) trait AgentAdapter: Sync {
    fn id(&self) -> &'static str;
    fn executable(&self) -> &'static str;
    /// Project-local installation recipe for this exact adapter executable.
    fn recipe(&self) -> Recipe;
    fn command(&self, context: &CommandContext<'_>) -> CommandSpec;
    fn prepare(&self, context: &mut PreparationContext<'_>) -> Result<(), Error>;
    fn collect(&self, _context: &CollectionContext<'_>) -> AgentArtifacts {
        AgentArtifacts::default()
    }
}

pub(crate) struct PreparationContext<'a> {
    pub command: &'a mut Command,
    pub provider: &'a str,
    pub model: &'a str,
    pub cli2key: Option<(&'a str, &'a str)>,
    /// The project-local tool root. Adapters choose their own configuration
    /// layout below this root instead of teaching the runner agent names.
    pub tools_dir: &'a Path,
    /// Canonical benchmark workspace for per-run adapter state.
    pub workspace: &'a Path,
}

pub(crate) struct CommandContext<'a> {
    pub config: &'a BenchmarkConfig,
    pub endpoint: Option<&'a str>,
    pub prompt: &'a str,
}

pub(crate) struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub append_prompt: bool,
}
#[derive(Default)]
pub(crate) struct AgentArtifacts {
    pub token_usage: Option<crate::measurements::TokenUsage>,
    /// Files that were consumed by this adapter and can be removed once their
    /// data has been incorporated into the generic run result.
    pub cleanup_paths: Vec<PathBuf>,
}

pub(crate) struct CollectionContext<'a> {
    pub workspace: &'a Path,
}

type CommandBuilder = for<'a> fn(&CommandContext<'a>) -> CommandSpec;
type PreparationHook = for<'a> fn(&mut PreparationContext<'a>) -> Result<(), Error>;
type CollectionHook = for<'a> fn(&CollectionContext<'a>) -> AgentArtifacts;

/// Trait-object implementation whose behavior is supplied by individual files.
pub(super) struct StaticAdapter {
    id: &'static str,
    executable: &'static str,
    recipe: Recipe,
    command: CommandBuilder,
    prepare: PreparationHook,
    collect: Option<CollectionHook>,
}
impl StaticAdapter {
    pub(super) const fn new(
        id: &'static str,
        executable: &'static str,
        recipe: Recipe,
        command: CommandBuilder,
        prepare: PreparationHook,
    ) -> Self {
        Self {
            id,
            executable,
            recipe,
            command,
            prepare,
            collect: None,
        }
    }

    pub(super) const fn with_collection(mut self, collect: CollectionHook) -> Self {
        self.collect = Some(collect);
        self
    }
}
impl AgentAdapter for StaticAdapter {
    fn id(&self) -> &'static str {
        self.id
    }
    fn executable(&self) -> &'static str {
        self.executable
    }
    fn recipe(&self) -> Recipe {
        self.recipe
    }
    fn command(&self, context: &CommandContext<'_>) -> CommandSpec {
        (self.command)(context)
    }
    fn prepare(&self, context: &mut PreparationContext<'_>) -> Result<(), Error> {
        (self.prepare)(context)
    }
    fn collect(&self, context: &CollectionContext<'_>) -> AgentArtifacts {
        self.collect
            .map(|collect| collect(context))
            .unwrap_or_default()
    }
}
pub(crate) fn prepare(
    command: &mut Command,
    agent: &str,
    provider: &str,
    model: &str,
    cli2key: Option<(&str, &str)>,
    tools_dir: &Path,
    workspace: &Path,
) -> Result<(), Error> {
    let Some(adapter) = find(agent) else {
        return Ok(());
    };
    adapter.prepare(&mut PreparationContext {
        command,
        provider,
        model,
        cli2key,
        tools_dir,
        workspace,
    })
}

static REGISTRY: &[&dyn AgentAdapter] = &[
    &codex::ADAPTER,
    &pi::ADAPTER,
    &opencode::ADAPTER,
    &workbuddy::ADAPTER,
    &qwen_code::ADAPTER,
    &mini_swe::ADAPTER,
    &openhands::ADAPTER,
    &openjiuwen::ADAPTER,
    &claude_code::ADAPTER,
    &goose::ADAPTER,
    &cline::ADAPTER,
    &continue_agent::ADAPTER,
];
pub(crate) fn all() -> &'static [&'static dyn AgentAdapter] {
    REGISTRY
}
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    REGISTRY.iter().map(|adapter| adapter.id())
}
pub(crate) fn find(id: &str) -> Option<&'static dyn AgentAdapter> {
    REGISTRY.iter().copied().find(|adapter| adapter.id() == id)
}

pub(crate) fn resolve_executable(
    id: &str,
    executable: &str,
    project_root: &Path,
    tools_dir: &str,
) -> Option<String> {
    let mut candidates = Vec::new();
    if let Some(directory) = std::env::var_os("ASB_TOOL_DIR") {
        candidates.push(std::path::PathBuf::from(directory).join(executable));
    }
    let tools_root = project::resolve(project_root, tools_dir);
    candidates.extend(executable_candidates(id, executable, &tools_root));
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| candidate.canonicalize().ok())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

/// Resolve an adapter's executable from a known project-local tools root.
/// The owning adapter uses this for its target-specific support executable.
pub(crate) fn resolve_from_tools(id: &str, tools_root: &Path) -> Option<String> {
    let adapter = find(id)?;
    executable_candidates(adapter.id(), adapter.executable(), tools_root)
        .into_iter()
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| candidate.canonicalize().ok())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

fn executable_candidates(id: &str, executable: &str, tools_root: &Path) -> Vec<PathBuf> {
    let root = tools_root.join(id);
    vec![
        root.join("bin").join(executable),
        root.join("node_modules/.bin").join(executable),
        root.join("venv/bin").join(executable),
        Path::new(".asb-tools/bin").join(executable),
    ]
}
pub(crate) fn resolve(id: &str, project_root: &Path, tools_dir: &str) -> Option<String> {
    let adapter = find(id)?;
    resolve_executable(adapter.id(), adapter.executable(), project_root, tools_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registered_adapter_builds_a_noninteractive_command() {
        let config = BenchmarkConfig {
            agent: "unused".into(),
            workload: "bug-fix".into(),
            provider: "cli2key".into(),
            model: "test-model".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
        };
        for adapter in all() {
            let command = adapter.command(&CommandContext {
                config: &config,
                endpoint: Some("http://127.0.0.1:3000/v1"),
                prompt: "repair",
            });
            assert!(!command.program.is_empty(), "{}", adapter.id());
            assert!(!command.args.is_empty(), "{}", adapter.id());
            assert!(!adapter.executable().is_empty(), "{}", adapter.id());
        }
    }

    #[test]
    fn registry_has_unique_stable_ids() {
        let ids = names().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), all().len());
        assert!(ids.contains("codex"));
        assert!(ids.contains("continue"));
    }
}
