//! Workload prompts loaded from the adjacent `workloads` directory.

use crate::suggestions;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn estimated_disk_mib(name: &str) -> u64 {
    adapters::find(name).map_or(1024, |adapter| adapter.estimated_disk_mib())
}

mod adapters;
pub(crate) mod contracts;
mod evaluation;
pub(crate) use evaluation::{KnownEvaluationKind, SavedEvaluationKind};

pub(crate) fn descriptor(name: &str) -> Option<contracts::ModuleDescriptor> {
    let descriptor = adapters::find(name).map(|adapter| adapter.descriptor(name));
    #[cfg(test)]
    let descriptor = crate::runner::fixture_module_descriptor(name, descriptor);
    descriptor
}

pub(crate) fn describe_module(
    name: &str,
    output: &mut dyn std::io::Write,
) -> Result<(), crate::Error> {
    let descriptor = descriptor(name)
        .filter(contracts::ModuleDescriptor::valid)
        .ok_or_else(|| {
            crate::Error::Input(format!("no benchmark module is registered for `{name}`"))
        })?;
    serde_json::to_writer_pretty(&mut *output, &descriptor)
        .map_err(|error| crate::Error::Io(error.to_string()))?;
    writeln!(output).map_err(crate::io_error)
}

pub(crate) struct PreflightContext<'a> {
    pub project_root: &'a Path,
    pub project_config: &'a crate::project::ProjectConfig,
    pub agent: &'a str,
}

impl PreflightContext<'_> {
    pub(crate) fn tool_root(&self, id: &str) -> PathBuf {
        crate::project::resolve(self.project_root, &self.project_config.tools_dir).join(id)
    }
}

pub(crate) fn preflight(name: &str, context: &PreflightContext<'_>) -> Result<(), crate::Error> {
    adapters::find(name).map_or(Ok(()), |adapter| adapter.preflight(context))
}
#[cfg(target_os = "linux")]
mod attributes;
#[cfg(target_os = "linux")]
pub(crate) mod census;
mod controller;
mod material;
pub(crate) mod outcome;
pub(crate) mod recovery;
pub(crate) mod relay;
mod remote;
mod selection;
pub(crate) mod session;
pub(crate) mod staging;
pub(crate) mod summary;
mod task_protocol;
pub(crate) fn runtime_inputs(
    name: &str,
    context: &crate::runtime_inputs::Context<'_>,
) -> Result<Vec<crate::runtime_inputs::Dependency>, crate::Error> {
    adapters::find(name).map_or_else(|| Ok(Vec::new()), |adapter| adapter.runtime_inputs(context))
}

/// Adapter-owned setup metadata. Callers must only retain a value while the
/// matching workload remains selected.
pub(crate) fn setup_extension(name: &str) -> Option<adapters::SetupExtension> {
    adapters::find(name).and_then(|adapter| adapter.setup_extension())
}

/// Enumerate adapter-owned choices for the active setup extension. Values are
/// stable domain identifiers; the UI reserves its own `[all]`
/// controls and never persists those labels as workload trials.
pub(crate) fn setup_extension_values(
    name: &str,
    extension: &adapters::SetupExtension,
    directory: &Path,
) -> Result<Vec<String>, String> {
    adapters::find(name)
        .ok_or_else(|| format!("no workload adapter is registered for `{name}`"))?
        .setup_extension_values(extension, directory)
}
pub(crate) mod target;
pub(crate) fn registered_sources() -> impl Iterator<Item = (&'static str, &'static str)> {
    adapters::sources()
}
pub(crate) use adapters::SourceBundle;
pub(crate) fn source_bundle(name: &str) -> Option<SourceBundle> {
    adapters::find(name).and_then(|adapter| adapter.source_bundle())
}
pub(crate) use selection::{PlanningCatalog, TaskSelection};
pub(crate) use session::BaselineEvidence;
pub use session::TaskIdentity;
pub(crate) use session::{
    ExecutionContext, PreparationContext, ResourceDisposition, WorkloadSession,
};

pub(crate) fn planning_catalog(
    name: &str,
    directory: &Path,
) -> Result<Option<PlanningCatalog>, String> {
    crate::selection_names::validate(name, "workload")?;
    let catalog =
        adapters::find(name).map_or(Ok(None), |adapter| adapter.planning_catalog(directory))?;
    if let Some(catalog) = &catalog {
        for task in &catalog.task_ids {
            crate::selection_names::validate(task, "trial")?;
        }
    }
    Ok(catalog)
}

pub(crate) fn describe_selection(
    config: &crate::benchmark::BenchmarkConfig,
    directory: &Path,
    catalog: Option<&PlanningCatalog>,
) -> Result<TaskSelection, String> {
    selection::describe(
        &config.workload,
        config.trial.as_deref(),
        config.trial_set.as_deref(),
        directory,
        catalog,
    )
}

pub(crate) struct InstallationContext<'a> {
    pub process: &'a crate::install::OperationContext,
    pub project_root: &'a Path,
    pub tools_root: &'a Path,
    pub workloads_root: &'a Path,
}

pub(crate) fn install_registered(
    name: &str,
    context: &InstallationContext<'_>,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> crate::install::InstallResult<bool> {
    adapters::find(name).map_or(Ok(false), |adapter| {
        adapter.install(context, output, progress)
    })
}

/// Let an adapter derive bounded, installed-source metadata before generic
/// installation publishes its default trial set.
pub(crate) fn finalize_install(name: &str, directory: &Path) -> Result<(), String> {
    adapters::find(name).map_or(Ok(()), |adapter| adapter.finalize_install(directory))
}

pub(crate) fn installed_available(name: &str, directory: &Path) -> Option<bool> {
    adapters::find(name).and_then(|adapter| adapter.installed_available(directory))
}

pub(crate) fn prepare_task(
    context: &PreparationContext<'_>,
) -> Result<Box<dyn WorkloadSession>, String> {
    if descriptor(context.workload).is_some_and(|descriptor| !descriptor.valid()) {
        return Err("workload module has an invalid descriptor".into());
    }
    adapters::find(context.workload)
        .ok_or_else(|| {
            format!(
                "no workload adapter is registered for `{}`",
                context.workload
            )
        })?
        .prepare_task(context)
}

const MAX_PROMPT_BYTES: u64 = 64 * 1024;

pub(crate) fn builtin_names() -> impl Iterator<Item = &'static str> {
    BUILTIN.iter().map(|(name, _)| *name)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialSet {
    pub workload: String,
    pub trials: Vec<Trial>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Trial {
    pub id: String,
    #[serde(default)]
    pub description: String,
}

struct Fixture {
    name: &'static str,
    files: &'static [(&'static str, &'static str)],
}

const BUILTIN: &[(&str, &str)] = &[
    (
        "bug-fix",
        include_str!("../../assets/workloads/bug-fix.txt"),
    ),
    (
        "build-repair",
        include_str!("../../assets/workloads/build-repair.txt"),
    ),
    (
        "dependency-migration",
        include_str!("../../assets/workloads/dependency-migration.txt"),
    ),
    (
        "feature-addition",
        include_str!("../../assets/workloads/feature-addition.txt"),
    ),
    (
        "refactoring",
        include_str!("../../assets/workloads/refactoring.txt"),
    ),
    (
        "repository-navigation",
        include_str!("../../assets/workloads/repository-navigation.txt"),
    ),
    (
        "test-generation",
        include_str!("../../assets/workloads/test-generation.txt"),
    ),
];

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "bug-fix",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/bug-fix/README.md"),
            ),
            (
                "src/parser.py",
                include_str!("../../assets/workloads/fixtures/bug-fix/src/parser.py"),
            ),
            (
                "tests/test_parser.py",
                include_str!("../../assets/workloads/fixtures/bug-fix/tests/test_parser.py"),
            ),
        ],
    },
    Fixture {
        name: "build-repair",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/build-repair/README.md"),
            ),
            (
                "src/app.py",
                include_str!("../../assets/workloads/fixtures/build-repair/src/app.py"),
            ),
            (
                "tests/test_app.py",
                include_str!("../../assets/workloads/fixtures/build-repair/tests/test_app.py"),
            ),
            (
                "build.sh",
                include_str!("../../assets/workloads/fixtures/build-repair/build.sh"),
            ),
        ],
    },
    Fixture {
        name: "dependency-migration",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/dependency-migration/README.md"),
            ),
            (
                "requirements.txt",
                include_str!(
                    "../../assets/workloads/fixtures/dependency-migration/requirements.txt"
                ),
            ),
            (
                "src/client.py",
                include_str!("../../assets/workloads/fixtures/dependency-migration/src/client.py"),
            ),
            (
                "tests/test_client.py",
                include_str!(
                    "../../assets/workloads/fixtures/dependency-migration/tests/test_client.py"
                ),
            ),
        ],
    },
    Fixture {
        name: "feature-addition",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/feature-addition/README.md"),
            ),
            (
                "src/formatter.py",
                include_str!("../../assets/workloads/fixtures/feature-addition/src/formatter.py"),
            ),
            (
                "tests/test_formatter.py",
                include_str!(
                    "../../assets/workloads/fixtures/feature-addition/tests/test_formatter.py"
                ),
            ),
        ],
    },
    Fixture {
        name: "refactoring",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/refactoring/README.md"),
            ),
            (
                "src/labels.py",
                include_str!("../../assets/workloads/fixtures/refactoring/src/labels.py"),
            ),
            (
                "tests/test_labels.py",
                include_str!("../../assets/workloads/fixtures/refactoring/tests/test_labels.py"),
            ),
        ],
    },
    Fixture {
        name: "repository-navigation",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/repository-navigation/README.md"),
            ),
            (
                "src/cli.py",
                include_str!("../../assets/workloads/fixtures/repository-navigation/src/cli.py"),
            ),
            (
                "src/registry.py",
                include_str!(
                    "../../assets/workloads/fixtures/repository-navigation/src/registry.py"
                ),
            ),
            (
                "tests/test_cli.py",
                include_str!(
                    "../../assets/workloads/fixtures/repository-navigation/tests/test_cli.py"
                ),
            ),
        ],
    },
    Fixture {
        name: "test-generation",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/test-generation/README.md"),
            ),
            (
                "src/temperature.py",
                include_str!("../../assets/workloads/fixtures/test-generation/src/temperature.py"),
            ),
            (
                "tests/test_temperature.py",
                include_str!(
                    "../../assets/workloads/fixtures/test-generation/tests/test_temperature.py"
                ),
            ),
        ],
    },
];

/// Populate a new project with the prompts shipped in the ASB binary.
pub(crate) fn populate(directory: &Path) -> Result<usize, std::io::Error> {
    adapters::populate_prompts(directory)
}

/// Generic prompt materialization used by the prompt-family adapter.
pub(super) fn populate_builtin_prompts(directory: &Path) -> Result<usize, std::io::Error> {
    let mut added = 0;
    for (name, prompt) in BUILTIN {
        let path = directory.join(format!("{name}.txt"));
        if !path.exists() {
            fs::write(path, prompt)?;
            added += 1;
        }
    }
    Ok(added)
}

/// Copy all bundled fixture templates into a project's managed fixture area.
pub(crate) fn populate_fixtures(directory: &Path) -> Result<usize, std::io::Error> {
    adapters::populate_fixtures(directory)
}

/// Generic fixture materialization used by the prompt-family adapter.
pub(super) fn populate_builtin_fixtures(directory: &Path) -> Result<usize, std::io::Error> {
    let mut added = 0;
    for fixture in FIXTURES {
        for (relative, contents) in fixture.files {
            let path = directory.join(fixture.name).join(relative);
            if !path.exists() {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, contents)?;
                if relative.ends_with(".sh") {
                    set_executable_io(&path)?;
                }
                added += 1;
            }
        }
    }
    Ok(added)
}

/// Materialize the selected fixture into the run workspace without overwriting edits.
#[cfg(test)]
pub(crate) fn prepare_fixture(name: &str, directory: &Path) -> Result<usize, String> {
    adapters::find(name).map_or(Ok(0), |adapter| adapter.prepare_fixture(name, directory))
}

pub(super) fn is_builtin_prompt(name: &str) -> bool {
    BUILTIN.iter().any(|(id, _)| *id == name)
}

/// Generic fixture setup used by the prompt-family adapter.
pub(super) fn prepare_builtin_fixture(name: &str, directory: &Path) -> Result<usize, String> {
    let Some(fixture) = FIXTURES.iter().find(|fixture| fixture.name == name) else {
        return Ok(0);
    };
    let mut added = 0;
    for (relative, contents) in fixture.files {
        let path = directory.join(relative);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    format!(
                        "cannot create fixture directory {}: {error}",
                        parent.display()
                    )
                })?;
            }
            fs::write(&path, contents)
                .map_err(|error| format!("cannot write fixture {}: {error}", path.display()))?;
            if relative.ends_with(".sh") {
                set_executable(&path)?;
            }
            added += 1;
        }
    }
    Ok(added)
}

/// Copy an installed external workload source into the run workspace.
#[cfg(test)]
pub(crate) fn prepare_external(
    name: &str,
    workload_directory: &Path,
    workspace: &Path,
) -> Result<usize, String> {
    if let Some(adapter) = adapters::find(name) {
        return adapter.prepare_source(workload_directory, workspace);
    }
    Ok(0)
}

/// Generic source materialization shared by external-suite adapters.
pub(crate) fn prepare_external_source(
    name: &str,
    workload_directory: &Path,
    workspace: &Path,
) -> Result<usize, String> {
    let source = workload_directory.join(name).join("source");
    let mut added = if source.is_dir() {
        copy_tree(&source, &workspace.join("external-workload"))?
    } else {
        0
    };
    let fixture = workload_directory.join(name).join("asb-trial");
    if fixture.is_dir() {
        added += copy_tree(
            &fixture,
            &workspace.join("external-workload").join("asb-trial"),
        )?;
    }
    Ok(added)
}

/// Materialize a deterministic, locally gradable adapter trial beside the
/// pinned upstream reference.  The reference remains available for context,
/// while this small case makes every external alias executable offline.
pub(crate) fn materialize_external_adapter(
    name: &str,
    directory: &Path,
) -> Result<(), std::io::Error> {
    if adapters::find(name).is_none() || is_builtin_prompt(name) {
        return Ok(());
    }
    let root = directory.join(name).join("asb-trial");
    let trial_label = adapter_trial_label(name);
    let files = [
        (
            "README.md",
            "# ASB adapter trial\n\nFix `src/task.py` so the supplied unit test passes. Preserve the suite-specific `suite_label()` value.\n",
        ),
        (
            "src/task.py",
            "def normalize(value: str) -> str:\n    return value.strip().replace(' ', '_')\n\ndef suite_label() -> str:\n    return \"__ASB_TRIAL_LABEL__\"\n",
        ),
        (
            "tests/test_task.py",
            "import unittest\nfrom src.task import normalize\n\nclass TaskTests(unittest.TestCase):\n    def test_normalize(self):\n        self.assertEqual(normalize(' Hello World '), 'hello-world')\n\nif __name__ == '__main__':\n    unittest.main()\n",
        ),
        ("src/__init__.py", ""),
        ("tests/__init__.py", ""),
    ];
    for (relative, contents) in files {
        let contents = contents.replace("__ASB_TRIAL_LABEL__", &trial_label);
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
    }
    fs::write(
        root.join("ASB-ADAPTER-FIXTURE"),
        format!("{name}/asb-adapter-trial-v1\n"),
    )?;
    fs::write(
        directory.join(format!("{name}.txt")),
        format!(
            "External workload adapter trial `{name}`. In `external-workload/asb-trial`, fix `src/task.py` so `normalize` returns lowercase hyphenated text and preserve suite_label()=`{trial_label}`. Do not edit the tests.\n"
        ),
    )?;
    Ok(())
}

fn adapter_trial_label(name: &str) -> String {
    name.replace('-', "_")
}

/// Return the kind of deterministic evaluation used for a workload.
///
/// External suite names are references to upstream suites, not claims that a
/// complete upstream benchmark has been executed.  Until an upstream suite
/// adapter is selected and installed, ASB runs the small adapter-smoke trial
/// that is materialized beside that reference.
pub(crate) fn evaluation_kind(name: &str, _workspace: &Path) -> KnownEvaluationKind {
    adapters::find(name).map_or(KnownEvaluationKind::Ungraded, |adapter| {
        adapter.evaluation_kind(name)
    })
}

/// Only an actual task evaluator can contribute to task-resolution rate.
/// A successful installation/protocol smoke exercise is never upstream credit.
pub(crate) fn resolution_eligible(kind: &str) -> bool {
    SavedEvaluationKind::classify(kind).resolution_eligible()
}

pub(crate) fn scored_kind<'a>(kind: &'a str, baseline: &session::BaselineEvidence) -> &'a str {
    SavedEvaluationKind::classify(kind)
        .scored(baseline)
        .as_str()
}

/// One attempt per selected task. Failures and absent grading stay in the
/// denominator; smoke checks and unknown provenance cannot earn task credit.
#[derive(Debug, Default, Serialize)]
pub(crate) struct ResolutionSummary {
    pub task_count: usize,
    pub resolved_count: usize,
    pub unresolved_count: usize,
    pub unscored_count: usize,
    pub success_rate: Option<f64>,
}

impl ResolutionSummary {
    pub fn observe(&mut self, kind: &str, resolved: bool) {
        if resolution_eligible(kind) {
            self.task_count += 1;
            self.resolved_count += usize::from(resolved);
        } else {
            self.unscored_count += 1;
        }
        self.unresolved_count = self.task_count - self.resolved_count;
        self.success_rate =
            (self.task_count > 0).then(|| self.resolved_count as f64 / self.task_count as f64);
    }
}

fn canonical_source(source: &str) -> String {
    source
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_owned()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Evaluation {
    Passed,
    Failed(String),
}

/// Evaluator diagnostics remain visible even when execution failed as well.
#[derive(Debug, Serialize)]
pub(crate) struct EvaluationFailure {
    pub kind: &'static str,
    pub reason: String,
}

/// Grade a run's materialized workload without treating a zero exit code as
/// task success.  Grading is implemented in Rust over evaluator-owned,
/// immutable expectations; no agent-controlled tests, subprocess, shell, or
/// inherited credentials are executed.
#[cfg(test)]
pub(crate) fn evaluate(
    name: &str,
    workspace: &Path,
    agent_output: &str,
) -> Result<Evaluation, String> {
    adapters::find(name)
        .ok_or_else(|| format!("no grader is available for workload `{name}`"))?
        .evaluator()(name, workspace, agent_output)
}

pub(super) fn evaluate_external_smoke(
    name: &str,
    workspace: &Path,
    _agent_output: &str,
) -> Result<Evaluation, String> {
    let directory = workspace.join("external-workload").join("asb-trial");
    let implementation = directory.join("src/task.py");
    let relative = "external-workload/asb-trial/src/task.py";
    let metadata = match fs::metadata(&implementation) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Evaluation::Failed(format!(
                "required adapter trial output `{relative}` is missing from the workload workspace; inspect the agent log and trial instructions"
            )));
        }
        Err(error) => {
            return Err(format!(
                "cannot inspect adapter trial output `{relative}`: {error}"
            ));
        }
    };
    if !metadata.is_file() {
        return Ok(Evaluation::Failed(format!(
            "adapter trial output `{relative}` is not a file"
        )));
    }
    if metadata.len() > MAX_PROMPT_BYTES {
        return Ok(Evaluation::Failed(format!(
            "adapter trial output `{relative}` exceeds the {MAX_PROMPT_BYTES} byte grading limit"
        )));
    }
    let source = fs::read_to_string(&implementation).map_err(|error| {
        format!(
            "cannot read adapter trial implementation {}: {error}",
            relative
        )
    })?;
    let expected_label = adapter_trial_label(name);
    let normalized = canonical_source(&source);
    // A substring search accepted dead code, early returns and string literals
    // containing a supposed solution. This is deliberately a bounded structural
    // smoke check, never official task evaluation or execution of agent code.
    let accepted = [
        "    return value.strip().lower().replace(' ', '-')",
        "    return value.strip().lower().replace(\" \", \"-\")",
        "    return '-'.join(value.lower().split())",
        "    return \"-\".join(value.lower().split())",
        "    return '-'.join(value.lower().replace('_', ' ').split())",
        "    return \"-\".join(value.lower().replace(\"_\", \" \").split())",
        "    return '-'.join(value.split()).lower()",
        "    return \"-\".join(value.split()).lower()",
        "    words = value.lower().split()\n    return '-'.join(words)",
        "    words = value.lower().split()\n    return \"-\".join(words)",
    ].iter().any(|body| ['\'', '"'].iter().any(|quote| {
        normalized == canonical_source(&format!("def normalize(value: str) -> str:\n{body}\n\ndef suite_label() -> str:\n    return {quote}{expected_label}{quote}\n"))
    }));
    Ok(if accepted {
        Evaluation::Passed
    } else {
        Evaluation::Failed(format!(
            "adapter-smoke output `{relative}` does not match the accepted trial implementation; check normalize and suite_label (equivalent solutions may not be recognized)"
        ))
    })
}

pub(crate) fn copy_tree(source: &Path, destination: &Path) -> Result<usize, String> {
    fs::create_dir_all(destination).map_err(|error| {
        format!(
            "cannot create external workload directory {}: {error}",
            destination.display()
        )
    })?;
    let mut added = 0;
    for relative in copy_plan(source)? {
        let source_path = source.join(&relative);
        let destination_path = destination.join(&relative);
        let kind = fs::symlink_metadata(&source_path).map_err(|error| {
            format!(
                "cannot inspect external workload entry {}: {error}",
                source_path.display()
            )
        })?;
        if kind.is_dir() {
            fs::create_dir_all(&destination_path).map_err(|error| {
                format!(
                    "cannot create external workload directory {}: {error}",
                    destination_path.display()
                )
            })?;
        } else if kind.is_file() && !destination_path.exists() {
            fs::copy(&source_path, &destination_path).map_err(|error| {
                format!(
                    "cannot copy external workload file {}: {error}",
                    source_path.display()
                )
            })?;
            added += 1;
        }
    }
    Ok(added)
}

/// Return the exact deterministic pre-order traversal used by `copy_tree`.
/// Sorting at every directory level prevents upstream archive extraction order
/// from changing the request body sent to a deterministic workload runner.
fn copy_plan(source: &Path) -> Result<Vec<PathBuf>, String> {
    fn visit(source: &Path, relative: &Path, plan: &mut Vec<PathBuf>) -> Result<(), String> {
        let mut entries = fs::read_dir(source)
            .map_err(|error| {
                format!(
                    "cannot read external workload source {}: {error}",
                    source.display()
                )
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("cannot inspect external workload source: {error}"))?;
        sort_entries(&mut entries);
        for entry in entries {
            let next = relative.join(entry.file_name());
            plan.push(next.clone());
            if entry
                .file_type()
                .map_err(|error| {
                    format!(
                        "cannot inspect external workload entry {}: {error}",
                        entry.path().display()
                    )
                })?
                .is_dir()
            {
                visit(&entry.path(), &next, plan)?;
            }
        }
        Ok(())
    }

    let mut plan = Vec::new();
    visit(source, Path::new(""), &mut plan)?;
    Ok(plan)
}

fn sort_entries(entries: &mut [fs::DirEntry]) {
    sort_by_name(entries, |entry| entry.file_name());
}

fn sort_by_name<T>(entries: &mut [T], name: impl Fn(&T) -> std::ffi::OsString) {
    entries.sort_by_key(name);
}

fn set_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .map_err(|error| format!("cannot inspect fixture {}: {error}", path.display()))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).map_err(|error| {
            format!("cannot make fixture {} executable: {error}", path.display())
        })?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn set_executable_io(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Load one workload prompt by name and report the available file names on a miss.
pub fn load(name: &str, directory: &Path) -> Result<String, String> {
    crate::selection_names::validate(name, "workload")?;
    let id = name.strip_prefix("original.").unwrap_or(name);
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(format!("invalid workload name `{name}`"));
    }
    let path = directory.join(format!("{id}.txt"));
    let metadata = fs::metadata(&path).map_err(|_| missing(name, directory))?;
    if !metadata.is_file() {
        return Err(missing(name, directory));
    }
    if metadata.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "workload `{name}` exceeds 64 KiB in {}",
            path.display()
        ));
    }
    let prompt = fs::read_to_string(&path).map_err(|error| {
        format!(
            "cannot read workload `{name}` from {}: {error}",
            path.display()
        )
    })?;
    if prompt.trim().is_empty() {
        return Err(format!("workload `{name}` is empty: {}", path.display()));
    }
    Ok(prompt)
}

pub fn load_trial(
    name: &str,
    trial: Option<&str>,
    trial_set: Option<&str>,
    directory: &Path,
) -> Result<String, String> {
    let prompt = load(name, directory)?;
    let Some(trial) = trial else {
        return Ok(prompt);
    };
    let selected = selected_trial(name, trial, trial_set, directory)?;
    let trial_prompt = directory
        .join(name)
        .join("trials")
        .join(format!("{}.txt", selected.id));
    if trial_prompt.is_file() {
        return load_prompt_file(&trial_prompt, name);
    }
    Ok(format!(
        "{prompt}\n\nSelected external workload trial: {}\n{}\n",
        selected.id, selected.description
    ))
}

/// Validate an explicit selection independently of prompt-file materialization.
pub(crate) fn selected_trial(
    name: &str,
    trial: &str,
    trial_set: Option<&str>,
    directory: &Path,
) -> Result<Trial, String> {
    let set_name = trial_set.ok_or_else(|| {
        format!(
            "workload `{name}` selected trial `{trial}` without a trial-set TOML; run `asb workload tui`"
        )
    })?;
    if Path::new(set_name).is_absolute()
        || Path::new(set_name)
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(format!("invalid workload trial-set path `{set_name}`"));
    }
    let config_path = directory.join(set_name);
    let config = read_trial_set(&config_path)?;
    if config.workload != name {
        return Err(format!(
            "trial set {} belongs to `{}`, not `{name}`",
            config_path.display(),
            config.workload
        ));
    }
    config
        .trials
        .into_iter()
        .find(|candidate| candidate.id == trial)
        .ok_or_else(|| {
            format!(
                "trial `{trial}` is not enabled in {}",
                config_path.display()
            )
        })
}

pub fn read_trial_set(path: &Path) -> Result<TrialSet, String> {
    if let Some(name) = path.file_name().and_then(|name| name.to_str()) {
        let name = name
            .strip_suffix(".workload.toml")
            .or_else(|| name.strip_suffix(".toml"))
            .unwrap_or(name);
        crate::selection_names::validate(name, "trial set")?;
    }
    let text = fs::read_to_string(path)
        .map_err(|error| format!("cannot read trial set {}: {error}", path.display()))?;
    let config: TrialSet = toml::from_str(&text)
        .map_err(|error| format!("invalid trial set {}: {error}", path.display()))?;
    crate::selection_names::validate(&config.workload, "workload")?;
    for trial in &config.trials {
        crate::selection_names::validate(&trial.id, "trial")?;
    }
    if config.workload.trim().is_empty() || config.trials.is_empty() {
        return Err(format!(
            "trial set {} must contain a workload and at least one trial",
            path.display()
        ));
    }
    Ok(config)
}

pub fn trial_ids(name: &str) -> Vec<Trial> {
    if let Some(adapter) = adapters::find(name) {
        return adapter.trial_ids();
    }
    let ids: &[(&str, &str)] = &[
        ("smoke", "Small local smoke trial."),
        ("standard", "Standard workload trial."),
        ("extended", "Extended workload trial."),
    ];
    ids.iter()
        .map(|(id, description)| Trial {
            id: (*id).into(),
            description: (*description).into(),
        })
        .collect()
}

/// Names backed by first-class external-suite adapters.
#[cfg(test)]
pub(crate) fn external_adapter_names() -> impl Iterator<Item = &'static str> {
    adapters::names()
}

/// Installed dataset tasks replace synthetic defaults; malformed data is an error.
pub fn installed_trials(name: &str, directory: &Path) -> Result<Vec<Trial>, String> {
    crate::selection_names::validate(name, "workload")?;
    let trials = adapters::find(name).map_or_else(
        || Ok(trial_ids(name)),
        |adapter| adapter.installed_trials(directory),
    )?;
    for trial in &trials {
        crate::selection_names::validate(&trial.id, "trial")?;
    }
    Ok(trials)
}

pub fn write_default_trial_set(name: &str, directory: &Path) -> Result<(), std::io::Error> {
    write_default_trial_set_with(name, directory, || {})
}

fn write_default_trial_set_with(
    name: &str,
    directory: &Path,
    before_publish: impl FnOnce(),
) -> Result<(), std::io::Error> {
    let path = directory.join(format!("{name}.workload.toml"));
    if path.exists() {
        return Ok(());
    }
    let config = TrialSet {
        workload: name.into(),
        trials: installed_trials(name, directory).map_err(std::io::Error::other)?,
    };
    let text = toml::to_string_pretty(&config).map_err(std::io::Error::other)?;
    before_publish();
    crate::config::matrix::scalar_if_absent(&path, text.as_bytes())
        .map_err(std::io::Error::other)?;
    Ok(())
}

fn load_prompt_file(path: &Path, name: &str) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| {
        format!(
            "cannot inspect trial `{name}` at {}: {error}",
            path.display()
        )
    })?;
    if metadata.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "trial `{name}` exceeds 64 KiB in {}",
            path.display()
        ));
    }
    let prompt = fs::read_to_string(path).map_err(|error| {
        format!(
            "cannot read trial `{name}` from {}: {error}",
            path.display()
        )
    })?;
    if prompt.trim().is_empty() {
        return Err(format!("trial `{name}` is empty: {}", path.display()));
    }
    Ok(prompt)
}

fn missing(name: &str, directory: &Path) -> String {
    let mut available = fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|value| value.to_str()) == Some("txt"))
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect::<Vec<_>>();
    available.sort();
    let candidates = available.iter().map(String::as_str).collect::<Vec<_>>();
    let suggestion = suggestions::closest(name, &candidates)
        .map(|candidate| format!("; did you mean `{candidate}`?"))
        .unwrap_or_default();
    let choices = if available.is_empty() {
        "none".to_owned()
    } else {
        available.join(", ")
    };
    format!(
        "workload `{name}` was not found in {}; available: {choices}{suggestion}",
        directory.display()
    )
}

#[cfg(test)]
#[path = "../../tests/unit/workload_boundaries.rs"]
mod boundary_tests;

#[cfg(test)]
#[path = "../../tests/unit/evaluation_kind_contract.rs"]
mod evaluation_kind_contract_tests;

#[cfg(test)]
mod tests {
    use super::{
        copy_plan, copy_tree, load, load_trial, materialize_external_adapter, populate,
        prepare_external, prepare_fixture,
    };
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn default_trial_set_does_not_overwrite_a_concurrent_user_selection() {
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        fs::create_dir_all(&root).unwrap();
        let destination = root.join("smoke.workload.toml");
        super::write_default_trial_set_with("smoke", &root, || {
            fs::write(&destination, "user selection").unwrap();
        })
        .unwrap();
        assert_eq!(fs::read_to_string(&destination).unwrap(), "user selection");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dependency_preflight_is_adapter_owned_and_runtime_discovery_is_read_only() {
        let root = std::env::temp_dir()
            .join(super::TaskIdentity::local("dependency-preflight", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let project = crate::project::ProjectConfig::default();
        let context = super::PreflightContext {
            project_root: &root,
            project_config: &project,
            agent: "missing-fixture-agent",
        };
        // Local tasks neither install remote tools nor require an agent for
        // dependency preparation. No environment/network mutation is needed.
        super::preflight("bug-fix", &context).unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        for (workload, sdk, helper) in [
            ("swe-bench", "swebench", "asb_swebench.py"),
            ("swe-bench-lite", "swebench", "asb_swebench.py"),
            ("swe-bench-verified", "swebench", "asb_swebench.py"),
            (
                "terminal-bench-1.0",
                "terminal-bench",
                "asb_terminal_bench_1.py",
            ),
            ("terminal-bench-2.0", "harbor", "asb_harbor.py"),
            ("terminal-bench-2.1", "harbor", "asb_harbor.py"),
        ] {
            let tool = context.tool_root(sdk);
            if !tool.join("venv/bin/python").exists() {
                let error = super::preflight(workload, &context).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains(&format!("asb workload install {workload}"))
                );
            }
            fs::create_dir_all(tool.join("venv/bin")).unwrap();
            fs::write(tool.join("venv/bin/python"), "fixture metadata only").unwrap();
            let helper = tool.join(helper);
            let inputs = super::runtime_inputs(
                workload,
                &crate::runtime_inputs::Context {
                    root: &root,
                    project: &project,
                },
            )
            .unwrap();
            assert!(!inputs.is_empty());
            // Metadata declarations do not publish helpers or download assets.
            let before = helper.exists();
            let error = super::preflight(workload, &context).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("selected agent has no installation adapter")
            );
            assert!(helper.is_file());
            // Terminal-Bench 2.0 and 2.1 deliberately share the Harbor helper,
            // just as the SWE variants share theirs.  Only the first variant is
            // expected to materialize a previously absent helper.
            if matches!(
                workload,
                "swe-bench" | "terminal-bench-1.0" | "terminal-bench-2.0"
            ) {
                assert!(!before);
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    fn evaluate(name: &str, workspace: &std::path::Path, output: &str) -> Result<bool, String> {
        super::evaluate(name, workspace, output).map(|value| value == super::Evaluation::Passed)
    }

    #[test]
    fn grade_explains_missing_wrong_large_and_unavailable_inputs() {
        use super::Evaluation;
        let root =
            std::env::temp_dir().join(format!("asb-diagnostic-grades-{}", std::process::id()));
        fs::create_dir_all(root.join("src")).unwrap();
        let grade = |name: &str, output: &str| super::evaluate(name, &root, output).unwrap();
        assert_eq!(
            grade("bug-fix", ""),
            Evaluation::Failed("required output file `src/parser.py` is missing from the workload workspace; inspect the agent log and workload instructions".into())
        );
        fs::write(root.join("src/parser.py"), "not the solution").unwrap();
        assert!(
            matches!(grade("bug-fix", ""), Evaluation::Failed(message) if message.contains("does not match an accepted solution") && !message.contains("not the solution"))
        );
        fs::write(
            root.join("src/parser.py"),
            vec![b'x'; super::MAX_PROMPT_BYTES as usize + 1],
        )
        .unwrap();
        assert!(
            matches!(grade("bug-fix", ""), Evaluation::Failed(message) if message.contains("exceeds") && message.contains("src/parser.py"))
        );
        assert!(
            matches!(grade("repository-navigation", "src/cli.py"), Evaluation::Failed(message) if message.contains("src/registry.py") && message.contains("lookup") && !message.contains("src/cli.py"))
        );
        assert!(
            super::evaluate("unknown", &root, "")
                .unwrap_err()
                .contains("no grader is available")
        );
        assert!(
            matches!(grade("agentbench", ""), Evaluation::Failed(message) if message.contains("adapter trial output") && message.contains("missing"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn loads_original_alias_and_rejects_path_traversal() {
        let directory = std::env::temp_dir().join(format!("asb-workloads-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("demo.txt"), "demo prompt").unwrap();
        assert_eq!(load("original.demo", &directory).unwrap(), "demo prompt");
        assert!(load("../secret", &directory).is_err());
        let _ = fs::remove_file(directory.join("demo.txt"));
        let _ = fs::remove_dir(directory);
    }

    #[test]
    fn populate_copies_bundled_prompts_without_overwriting_custom_files() {
        let directory =
            std::env::temp_dir().join(format!("asb-bundled-workloads-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("bug-fix.txt"), "custom").unwrap();
        let added = populate(&directory).expect("populate succeeds");
        assert_eq!(added, 6);
        assert_eq!(
            fs::read_to_string(directory.join("bug-fix.txt")).unwrap(),
            "custom"
        );
        assert!(
            !fs::read_to_string(directory.join("repository-navigation.txt"))
                .unwrap()
                .is_empty()
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn every_builtin_fixture_materializes_supporting_files() {
        let directory = std::env::temp_dir().join(format!("asb-fixtures-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        for name in [
            "bug-fix",
            "build-repair",
            "dependency-migration",
            "feature-addition",
            "refactoring",
            "repository-navigation",
            "test-generation",
        ] {
            let fixture_dir = directory.join(name);
            assert!(prepare_fixture(name, &fixture_dir).unwrap() > 0, "{name}");
            assert!(fixture_dir.join("README.md").is_file(), "{name} README");
            assert!(fixture_dir.join("tests").is_dir(), "{name} tests");
        }
        assert!(directory.join("build-repair/build.sh").is_file());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn installed_external_source_is_copied_without_overwriting_workspace_edits() {
        let root = std::env::temp_dir().join(format!("asb-external-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("workloads/agentbench/source");
        let workspace = root.join("workspace");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("README.md"), "upstream").unwrap();
        fs::create_dir_all(workspace.join("external-workload")).unwrap();
        fs::write(workspace.join("external-workload/README.md"), "edited").unwrap();
        assert_eq!(
            prepare_external("agentbench", &root.join("workloads"), &workspace).unwrap(),
            0
        );
        assert_eq!(
            fs::read_to_string(workspace.join("external-workload/README.md")).unwrap(),
            "edited"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn external_source_copy_uses_deterministic_lexical_traversal() {
        let root =
            std::env::temp_dir().join(format!("asb-external-copy-order-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("source");
        let destination = root.join("destination");
        fs::create_dir_all(source.join("z-last/inner-z")).unwrap();
        fs::create_dir_all(source.join("a-first/inner-z")).unwrap();
        fs::write(source.join("z-last/file"), "z").unwrap();
        fs::write(source.join("a-first/file"), "a").unwrap();
        fs::write(source.join("z-last/inner-z/file"), "z-inner").unwrap();
        fs::write(source.join("a-first/inner-z/file"), "a-inner").unwrap();

        // The explicit unsorted input proves the negative case without
        // depending on unspecified filesystem directory iteration order.
        let mut unsorted = vec![PathBuf::from("z-last"), PathBuf::from("a-first")];
        super::sort_by_name(&mut unsorted, |path| path.clone().into_os_string());
        assert_eq!(
            unsorted,
            vec![PathBuf::from("a-first"), PathBuf::from("z-last")]
        );
        assert_eq!(
            copy_plan(&source).unwrap(),
            vec![
                PathBuf::from("a-first"),
                PathBuf::from("a-first/file"),
                PathBuf::from("a-first/inner-z"),
                PathBuf::from("a-first/inner-z/file"),
                PathBuf::from("z-last"),
                PathBuf::from("z-last/file"),
                PathBuf::from("z-last/inner-z"),
                PathBuf::from("z-last/inner-z/file"),
            ]
        );
        copy_tree(&source, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("a-first/inner-z/file")).unwrap(),
            "a-inner"
        );
        assert_eq!(
            fs::read_to_string(destination.join("z-last/inner-z/file")).unwrap(),
            "z-inner"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn external_adapter_trial_is_materialized_and_graded() {
        let root = std::env::temp_dir().join(format!("asb-external-trial-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workloads = root.join("workloads");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workloads).unwrap();
        materialize_external_adapter("agentbench", &workloads).unwrap();
        prepare_external("agentbench", &workloads, &workspace).unwrap();
        let task = workspace.join("external-workload/asb-trial/src/task.py");
        assert!(!evaluate("agentbench", &workspace, "").unwrap());
        for source in [
            "def normalize(value: str) -> str:\n    return value\n    return value.strip().lower().replace(' ', '-')\n\ndef suite_label() -> str:\n    return \"agentbench\"\n",
            "def normalize(value: str) -> str:\n    example = \"value.lower().replace(' ', '-')\"\n    return value\n\ndef suite_label() -> str:\n    return \"agentbench\"\n",
            "def normalize(value: str) -> str:\n    return value.strip().lower().replace(' ', '-')\n\ndef suite_label() -> str:\n    return \"agentbench\"\n\ndef normalize(value: str) -> str:\n    return value\n",
        ] {
            fs::write(&task, source).unwrap();
            assert!(!evaluate("agentbench", &workspace, "").unwrap());
        }
        fs::write(
            &task,
            "def normalize(value: str) -> str:\n    return value.strip().lower().replace(' ', '-')\n\ndef suite_label() -> str:\n    return \"agentbench\"\n",
        )
        .unwrap();
        // The grader does not execute or read agent-controlled tests.
        fs::write(
            workspace.join("external-workload/asb-trial/tests/test_task.py"),
            "# intentionally tampered by the agent\n",
        )
        .unwrap();
        assert!(evaluate("agentbench", &workspace, "").unwrap());
        fs::write(
            &task,
            "def normalize(value: str) -> str:\n    return '-'.join(value.lower().replace('_', ' ').split())\n\ndef suite_label() -> str:\n    return \"agentbench\"\n",
        )
        .unwrap();
        assert!(evaluate("agentbench", &workspace, "").unwrap());
        for implementation in [
            "def normalize(value: str) -> str:\n    return '-'.join(value.split()).lower()\n\ndef suite_label() -> str:\n    return \"agentbench\"\n",
            "def normalize(value: str) -> str:\n    words = value.lower().split()\n    return \"-\".join(words)\n\ndef suite_label() -> str:\n    return \"agentbench\"\n",
        ] {
            fs::write(&task, implementation).unwrap();
            assert!(evaluate("agentbench", &workspace, "").unwrap());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_grader_rejects_the_unfixed_fixture() {
        let root = std::env::temp_dir().join(format!("asb-builtin-grade-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        prepare_fixture("bug-fix", &workspace).unwrap();
        assert!(!evaluate("bug-fix", &workspace, "").unwrap());
        fs::write(
            workspace.join("src/parser.py"),
            "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n",
        )
        .unwrap();
        assert!(evaluate("bug-fix", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_grader_rejects_dead_or_unused_markers() {
        let root =
            std::env::temp_dir().join(format!("asb-builtin-semantic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        prepare_fixture("feature-addition", &workspace).unwrap();
        fs::write(
            workspace.join("src/formatter.py"),
            "def format_items(items: list[str]) -> str:\n    reverse: bool = False\n    if False:\n        return \", \".join(reversed(items))\n    return \", \".join(items)\n",
        )
        .unwrap();
        assert!(!evaluate("feature-addition", &workspace, "").unwrap());
        fs::write(
            workspace.join("src/formatter.py"),
            "def format_items(items: list[str], reverse: bool = False) -> str:\nordered = list(reversed(items)) if reverse else items\n    return \", \".join(ordered)\n",
        )
        .unwrap();
        assert!(!evaluate("feature-addition", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_grader_accepts_the_equivalent_inline_feature_solution() {
        let root = std::env::temp_dir().join(format!("asb-feature-variant-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        prepare_fixture("feature-addition", &workspace).unwrap();
        fs::write(
            workspace.join("src/formatter.py"),
            "def format_items(items: list[str], reverse: bool = False) -> str:\n    return \", \".join(reversed(items) if reverse else items)\n",
        )
        .unwrap();
        assert!(evaluate("feature-addition", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_grader_accepts_valid_refactoring_and_test_generation_variants() {
        let root =
            std::env::temp_dir().join(format!("asb-builtin-variants-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let refactoring_workspace = root.join("refactoring");
        prepare_fixture("refactoring", &refactoring_workspace).unwrap();
        fs::write(
            refactoring_workspace.join("src/labels.py"),
            "def _join_labels(*parts: str) -> str:\n    return \":\".join(f\"{part}\" for part in parts)\n\ndef label_pair(left: str, right: str) -> str:\n    return _join_labels(left, right)\n\ndef label_triplet(first: str, second: str, third: str) -> str:\n    return _join_labels(first, second, third)\n",
        )
        .unwrap();
        assert!(evaluate("refactoring", &refactoring_workspace, "").unwrap());

        let test_generation_workspace = root.join("test-generation");
        prepare_fixture("test-generation", &test_generation_workspace).unwrap();
        fs::write(
            test_generation_workspace.join("tests/test_temperature.py"),
            "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        for celsius, expected in ((-0.1, \"freezing\"), (0, \"mild\"), (0.1, \"mild\")):\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        for celsius, expected in ((29.9, \"mild\"), (30, \"mild\"), (30.1, \"hot\")):\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
        )
        .unwrap();
        assert!(evaluate("test-generation", &test_generation_workspace, "").unwrap());
        fs::write(
            test_generation_workspace.join("tests/test_temperature.py"),
            "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        for celsius, expected in [\n            (-0.1, \"freezing\"),\n            (0, \"mild\"),\n            (0.1, \"mild\"),\n        ]:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        for celsius, expected in [\n            (29.9, \"mild\"),\n            (30, \"mild\"),\n            (30.1, \"hot\"),\n        ]:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
        )
        .unwrap();
        assert!(evaluate("test-generation", &test_generation_workspace, "").unwrap());
        fs::write(
            test_generation_workspace.join("tests/test_temperature.py"),
            "import math\n\nimport unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        cases = (\n            (math.nextafter(0.0, -math.inf), \"freezing\"),\n            (0.0, \"mild\"),\n            (math.nextafter(0.0, math.inf), \"mild\"),\n        )\n        for celsius, expected in cases:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        cases = (\n            (math.nextafter(30.0, -math.inf), \"mild\"),\n            (30.0, \"mild\"),\n            (math.nextafter(30.0, math.inf), \"hot\")\n        )\n        for celsius, expected in cases:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
        )
        .unwrap();
        assert!(evaluate("test-generation", &test_generation_workspace, "").unwrap());
        fs::write(
            test_generation_workspace.join("tests/test_temperature.py"),
            "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_mild_lower_boundary(self):\n        self.assertEqual(classify(0), \"mild\")\n\n    def test_mild_upper_boundary(self):\n        self.assertEqual(classify(30), \"mild\")\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
        )
        .unwrap();
        assert!(evaluate("test-generation", &test_generation_workspace, "").unwrap());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_grader_accepts_multiline_tuple_test_generation_variant() {
        let root = std::env::temp_dir().join(format!(
            "asb-test-generation-multiline-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        prepare_fixture("test-generation", &workspace).unwrap();
        fs::write(
            workspace.join("tests/test_temperature.py"),
            "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        for celsius, expected in (\n            (-0.1, \"freezing\"),\n            (0, \"mild\"),\n            (0.1, \"mild\"),\n        ):\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        for celsius, expected in (\n            (29.9, \"mild\"),\n            (30, \"mild\"),\n            (30.1, \"hot\"),\n        ):\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
        )
        .unwrap();
        assert!(evaluate("test-generation", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_grader_accepts_single_line_list_test_generation_variant() {
        let root = std::env::temp_dir().join(format!(
            "asb-test-generation-list-single-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        prepare_fixture("test-generation", &workspace).unwrap();
        fs::write(
            workspace.join("tests/test_temperature.py"),
            "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        for celsius, expected in [(-0.1, \"freezing\"), (0, \"mild\"), (0.1, \"mild\")]:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        for celsius, expected in [(29.9, \"mild\"), (30, \"mild\"), (30.1, \"hot\")]:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
        )
        .unwrap();
        assert!(evaluate("test-generation", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repository_navigation_grades_only_the_explicit_final_response() {
        let root =
            std::env::temp_dir().join(format!("asb-navigation-grade-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        prepare_fixture("repository-navigation", &workspace).unwrap();
        let tool_trace = "unrelated final answer";
        assert!(!evaluate("repository-navigation", &workspace, tool_trace).unwrap());
        let answer = "The call path goes from src/cli.py to src/registry.py through lookup.";
        assert!(evaluate("repository-navigation", &workspace, answer).unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn external_grader_rejects_unreachable_adapter_code() {
        let root =
            std::env::temp_dir().join(format!("asb-external-semantic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workloads = root.join("workloads");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workloads).unwrap();
        materialize_external_adapter("agentbench", &workloads).unwrap();
        prepare_external("agentbench", &workloads, &workspace).unwrap();
        let task = workspace.join("external-workload/asb-trial/src/task.py");
        fs::write(
            &task,
            "def normalize(value: str) -> str:\n    if False:\n        return value.strip().lower().replace(' ', '-')\n    return value\n\ndef suite_label() -> str:\n    return \"agentbench\"\n",
        )
        .unwrap();
        assert!(!evaluate("agentbench", &workspace, "").unwrap());
        fs::write(
            &task,
            "def normalize(value: str) -> str:\n    return value.strip().lower().replace('     ', '-')\n\ndef suite_label() -> str:\n    return \"agentbench\"\n",
        )
        .unwrap();
        assert!(!evaluate("agentbench", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_workload_suggests_a_close_local_name() {
        let directory =
            std::env::temp_dir().join(format!("asb-workload-suggestion-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("bug-fix.txt"), "prompt").unwrap();
        let error = load("bug-fx", &directory).expect_err("typo rejected");
        assert!(error.contains("did you mean `bug-fix`?"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn trial_sets_round_trip_and_select_a_configured_prompt() {
        let directory = std::env::temp_dir().join(format!("asb-trials-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(directory.join("bug-fix/trials")).unwrap();
        fs::write(directory.join("bug-fix.txt"), "base prompt").unwrap();
        fs::write(
            directory.join("bug-fix.workload.toml"),
            "workload = 'bug-fix'\n[[trials]]\nid = 'debug'\ndescription = 'debug task'\n",
        )
        .unwrap();
        fs::write(directory.join("bug-fix/trials/debug.txt"), "debug task").unwrap();
        let prompt = load_trial(
            "bug-fix",
            Some("debug"),
            Some("bug-fix.workload.toml"),
            &directory,
        )
        .unwrap();
        assert_eq!(prompt, "debug task");
        assert!(
            load_trial(
                "bug-fix",
                Some("unknown"),
                Some("bug-fix.workload.toml"),
                &directory,
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn selected_manifest_is_checked_without_loading_a_prompt_or_environment() {
        let root = std::env::temp_dir().join(format!(
            "asb-selection-{}",
            super::TaskIdentity::local("selection", None).attempt_id
        ));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("selection.toml"),
            "workload='official-suite'\n[[trials]]\nid='case-1'\ndescription='Selected case'\n",
        )
        .unwrap();
        assert_eq!(
            super::selected_trial("official-suite", "case-1", Some("selection.toml"), &root)
                .unwrap()
                .id,
            "case-1"
        );
        assert!(
            super::selected_trial("official-suite", "case-2", Some("selection.toml"), &root)
                .unwrap_err()
                .contains("not enabled")
        );
        assert!(
            super::selected_trial("different-suite", "case-1", Some("selection.toml"), &root)
                .unwrap_err()
                .contains("belongs to")
        );
        for unsafe_path in ["../selection.toml", "/selection.toml"] {
            assert!(
                super::selected_trial("official-suite", "case-1", Some(unsafe_path), &root)
                    .unwrap_err()
                    .contains("invalid workload trial-set path")
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
#[test]
fn task_resolution_counts_failures_and_excludes_smoke_and_unknown() {
    let mut summary = ResolutionSummary::default();
    summary.observe("adapter-smoke", true);
    summary.observe("unknown", true);
    assert_eq!(summary.success_rate, None);
    summary.observe("bundled-fixture", true);
    summary.observe("bundled-fixture", false);
    summary.observe("bundled-fixture", false); // agent/grader failure
    assert_eq!(summary.task_count, 3);
    assert_eq!(summary.resolved_count, 1);
    assert_eq!(summary.unresolved_count, 2);
    assert_eq!(summary.unscored_count, 2);
    assert_eq!(summary.success_rate, Some(1.0 / 3.0));
    for name in adapters::names() {
        assert!(
            !evaluation_kind(name, std::path::Path::new("")).resolution_eligible(),
            "{name} is only an adapter smoke fixture"
        );
    }
}
