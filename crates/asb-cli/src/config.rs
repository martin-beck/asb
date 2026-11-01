//! The setup command and its small file format.

use super::Error;
use crate::agents;
use crate::benchmark::{BenchmarkConfig, MeasurementConfig, RetentionConfig};
use crate::measurements;
use crate::project;
use crate::providers;
use crate::suggestions;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct SetupOptions {
    pub agent: Vec<String>,
    pub workload: Vec<String>,
    pub trial: Vec<String>,
    pub trial_set: Vec<String>,
    pub provider: Vec<String>,
    pub model: Vec<String>,
    pub measure: Vec<String>,
    pub concurrency: Option<usize>,
    pub workspace: Option<String>,
    pub output: Option<String>,
    pub retention: RetentionConfig,
    /// Repeat default persisted when this setup produces a sweep manifest.
    pub repeat: Option<u64>,
}

/// A materialized preset sweep together with its declared execution defaults.
pub(crate) struct PresetSweep {
    pub(crate) path: PathBuf,
    pub(crate) repeat: u64,
}

/// Expand a repository-owned, machine-readable benchmark preset.
pub fn setup_preset(
    name: &str,
    output_path: Option<String>,
    output: &mut dyn Write,
) -> Result<(), Error> {
    let mut options = crate::presets::load(name)?;
    if output_path.is_some() {
        options.output = output_path;
    }
    matrix::setup(options, output)
}

/// Write a TOML benchmark configuration from parsed setup options.
pub fn setup(options: SetupOptions, output: &mut dyn Write) -> Result<(), Error> {
    matrix::setup(options, output)
}

/// Materialize a repository-owned preset in an initialized project so it can
/// immediately be executed as a sweep.
///
/// Unlike `asb setup --preset`, this owns a fresh internal output path.  The
/// generated TOMLs remain in the project as the durable execution input and
/// provenance for the resulting sweep.
pub(crate) fn setup_preset_for_sweep(
    name: &str,
    quiet: bool,
    output: &mut dyn Write,
) -> Result<PresetSweep, Error> {
    let (root, _) = project::load(Path::new("."))?;
    require_initialized_preset_project(&root, name)?;
    let defaults = crate::presets::sweep_defaults(name)?;
    let mut options = crate::presets::load(name)?;
    let path = preset_sweep_path(&root, name, matrix::stamp());
    options.output = Some(path.to_string_lossy().into_owned());
    if quiet {
        matrix::setup(options, &mut std::io::sink())?;
    } else {
        matrix::setup(options, output)?;
    }
    Ok(PresetSweep {
        path,
        repeat: defaults.repeat,
    })
}

fn require_initialized_preset_project(root: &Path, name: &str) -> Result<(), Error> {
    if root.join(".asb/project.toml").is_file() {
        Ok(())
    } else {
        Err(Error::Input(format!(
            "project is not initialized; run `asb init` before `asb sweep --preset {name}`"
        )))
    }
}

fn preset_sweep_path(root: &Path, name: &str, stamp: u128) -> PathBuf {
    let stem = name.strip_suffix(".toml").unwrap_or(name);
    root.join(".asb")
        .join("preset-sweeps")
        .join(format!("{stem}-{stamp}.toml"))
}

/// Validate one selection before publishing any configuration in a matrix.
fn configuration(options: SetupOptions) -> Result<BenchmarkConfig, Error> {
    let SetupOptions {
        agent,
        workload,
        trial,
        trial_set,
        provider,
        model,
        measure,
        concurrency,
        workspace,
        output: destination,
        retention,
        repeat: _,
    } = options;
    let (_, project_config) = project::load(std::path::Path::new("."))?;
    let agent = required(agent.into_iter().next())?;
    let known_agents = agents::all()
        .iter()
        .map(|adapter| adapter.id())
        .collect::<Vec<_>>();
    let adapter = agents::find(&agent).ok_or_else(|| {
        let suggestion = suggestions::closest(&agent, &known_agents)
            .map(|candidate| format!("; did you mean `{candidate}`?"))
            .unwrap_or_default();
        Error::Input(format!("unknown agent `{agent}`{suggestion}"))
    })?;
    let provider = required(provider.into_iter().next())?;
    providers::model_spec(&provider).ok_or_else(|| {
        let suggestion = suggestions::closest(&provider, providers::names())
            .map(|candidate| format!("; did you mean `{candidate}`?"))
            .unwrap_or_default();
        Error::Input(format!("unknown provider `{provider}`{suggestion}"))
    })?;
    let model = required(model.into_iter().next())?;
    for (name, kind) in [
        (&agent, "agent"),
        (&provider, "provider"),
        (&model, "model"),
    ] {
        crate::selection_names::validate(name, kind).map_err(Error::Input)?;
    }
    let concurrency = concurrency.unwrap_or(1);
    if concurrency == 0 {
        return Err(Error::Input("concurrency must be at least 1".into()));
    }
    if concurrency > crate::benchmark::MAX_CONCURRENCY {
        return Err(Error::Input(format!(
            "concurrency must not exceed {}; split larger selections into bounded sweeps",
            crate::benchmark::MAX_CONCURRENCY
        )));
    }
    let config = BenchmarkConfig {
        planning: None,
        agent,
        workload: required(workload.into_iter().next())?,
        provider,
        model,
        concurrency,
        trial_set: trial_set.into_iter().next(),
        trial: trial.into_iter().next(),
        workspace: workspace.unwrap_or(project_config.workspace_dir),
        log_file: destination
            .as_deref()
            .map(default_log_name)
            .unwrap_or_else(|| crate::benchmark::default_log_path("benchmark.log")),
        measurements: Some(MeasurementConfig {
            enabled: true,
            directory: "measurements".into(),
            collectors: if measure.is_empty() {
                measurements::available_collectors()
            } else {
                crate::collectors::expand_dependencies(&measure)
            },
        }),
        retention,
        mcp_servers: Vec::new(),
    };
    // Reject static endpoint mismatches before publishing a configuration.
    // Live model-catalog tool support is checked before the provider/agent starts.
    crate::endpoint::choose_from(
        adapter.id(),
        adapter.supported_endpoints(),
        &config.provider,
        &config.model,
        &providers::offered_endpoints(&config.provider, &config.model),
    )?;
    Ok(config)
}

fn validate_selection(
    config: &BenchmarkConfig,
    directory: &std::path::Path,
    catalog: Option<&crate::workloads::PlanningCatalog>,
    enabled_trials: Option<&std::collections::BTreeSet<String>>,
) -> Result<(), Error> {
    if config.trial_set.is_some() && config.trial.is_none() {
        return Err(Error::Input(
            "--trial-set requires --trial; use `asb setup tui` to select multiple instances".into(),
        ));
    }
    if catalog.is_some() && config.trial.is_none() {
        return Err(Error::Input(format!(
            "workload `{}` requires --trial INSTANCE; choose an installed instance with `asb workload tui` or use `asb setup tui`",
            config.workload
        )));
    }
    if catalog.is_none() && config.trial.is_some() && config.trial_set.is_none() {
        return Err(Error::Input("--trial requires --trial-set for a workload without an installed instance catalog; select trials with `asb workload tui` first".into()));
    }
    if let Some(enabled) = enabled_trials
        && config
            .trial
            .as_ref()
            .is_none_or(|trial| !enabled.contains(trial))
    {
        return Err(Error::Input(format!(
            "trial `{}` is not enabled in {}",
            config.trial.as_deref().unwrap_or(""),
            config
                .trial_set
                .as_deref()
                .unwrap_or("the selected trial set")
        )));
    }
    let mut validated = config.clone();
    // The owning set and membership were loaded/checked once for this matrix.
    // Do not parse an entire population file again for every agent/model pair.
    if enabled_trials.is_some() {
        validated.trial_set = None;
    }
    crate::workloads::describe_selection(&validated, directory, catalog).map_err(Error::Input)?;
    crate::planning::resolve(config)?;
    Ok(())
}

pub(crate) mod matrix;

fn default_log_name(destination: &str) -> String {
    std::path::Path::new(destination)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(|value| crate::benchmark::default_log_path(&format!("{value}.log")))
        .unwrap_or_else(|| crate::benchmark::default_log_path("benchmark.log"))
}

fn run_command_message(path: &str) -> String {
    format!("Run it with: asb run {path}")
}

fn required(value: Option<String>) -> Result<String, Error> {
    let value = value.ok_or(Error::Usage(
        "setup requires agent, workload, provider, and model",
    ))?;
    if value.trim().is_empty() {
        return Err(Error::Input("setup values cannot be empty".into()));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{
        SetupOptions, default_log_name, preset_sweep_path, require_initialized_preset_project,
        run_command_message, setup,
    };
    use std::path::Path;

    mod setup_installables {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/setup_installables.rs"
        ));
    }

    #[test]
    fn generated_preset_sweeps_stay_under_the_initialized_project() {
        assert_eq!(
            preset_sweep_path(Path::new("project"), "harnesstax.toml", 42),
            Path::new("project/.asb/preset-sweeps/harnesstax-42.toml")
        );
    }

    #[test]
    fn preset_sweep_requires_a_managed_project() {
        let root = std::env::temp_dir().join(format!("asb-preset-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let error = require_initialized_preset_project(&root, "harnesstax.toml").unwrap_err();
        assert!(error.to_string().contains("asb init"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_derives_log_name_from_configuration_stem() {
        assert_eq!(default_log_name("configs/foo.toml"), ".asb-logs/foo.log");
        assert_eq!(default_log_name("benchmark"), ".asb-logs/benchmark.log");
    }

    #[test]
    fn setup_output_explains_how_to_run_written_configuration() {
        assert_eq!(
            run_command_message("configs/foo.toml"),
            "Run it with: asb run configs/foo.toml"
        );
    }

    #[test]
    fn setup_writes_default_concurrency() {
        const CHILD: &str = "ASB_SETUP_DEFAULT_CONCURRENCY_FIXTURE";
        if std::env::var_os(CHILD).is_none() {
            let root = std::env::temp_dir().join(format!(
                "asb-setup-default-{}",
                crate::tools::runtime::unique_id()
            ));
            std::fs::create_dir_all(root.join(".asb")).unwrap();
            std::fs::write(
                root.join(".asb/project.toml"),
                toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
            )
            .unwrap();
            std::fs::create_dir_all(root.join("workloads")).unwrap();
            crate::workloads::populate(&root.join("workloads")).unwrap();
            setup_installables::seed(&root, ".asb-tools");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .current_dir(&root)
                .env(CHILD, "1")
                .args(["--exact", "config::tests::setup_writes_default_concurrency"])
                .output()
                .unwrap();
            let _ = std::fs::remove_dir_all(&root);
            assert!(output.status.success(), "child setup fixture failed");
            return;
        }
        let mut output = Vec::new();
        setup(options("pi", "cli2key", "gpt-6.1-sol"), &mut output).expect("setup succeeds");
        let text = String::from_utf8(output).expect("setup is UTF-8");
        assert!(text.contains("concurrency = 1"));
        let config: crate::benchmark::BenchmarkConfig = toml::from_str(&text).unwrap();
        let selected = config.measurements.unwrap();
        assert!(selected.enabled);
        assert_eq!(
            selected.collectors,
            crate::measurements::available_collectors()
        );
    }

    #[test]
    fn setup_carries_compact_retention_into_the_written_configuration() {
        let mut selection = options("pi", "cli2key", "gpt-6.1-sol");
        selection.retention.compact_completed_attempts = true;
        let config = super::configuration(selection).expect("configuration succeeds");
        assert!(config.retention.compact_completed_attempts);
    }

    #[test]
    fn setup_rejects_unbounded_concurrency_before_writing_configuration() {
        let mut selection = options("pi", "cli2key", "fixture");
        selection.concurrency = Some(usize::MAX);
        let mut output = Vec::new();
        assert!(
            setup(selection, &mut output)
                .unwrap_err()
                .to_string()
                .contains("must not exceed 64")
        );
        assert!(output.is_empty());
    }

    fn options(agent: &str, provider: &str, model: &str) -> SetupOptions {
        SetupOptions {
            agent: vec![agent.into()],
            workload: vec!["bug-fix".into()],
            trial: vec![],
            trial_set: vec![],
            provider: vec![provider.into()],
            model: vec![model.into()],
            measure: vec![],
            concurrency: None,
            workspace: None,
            output: None,
            retention: crate::benchmark::RetentionConfig::default(),
            repeat: None,
        }
    }

    #[test]
    fn setup_suggests_close_agent_and_provider_names() {
        let agent_error = setup(options("opencod", "local", "test"), &mut Vec::new())
            .expect_err("agent typo rejected");
        assert!(agent_error.to_string().contains("did you mean `opencode`?"));
        let provider_error = setup(options("opencode", "openruter", "test"), &mut Vec::new())
            .expect_err("provider typo rejected");
        assert!(
            provider_error
                .to_string()
                .contains("did you mean `openrouter`?")
        );
        setup(
            options("opencode", "cli2key", "new-live-model"),
            &mut Vec::new(),
        )
        .expect("setup must not reject models absent from an old snapshot");
    }

    #[test]
    fn setup_rejects_unknown_names_and_incompatible_endpoint_pairs() {
        let unknown = setup(
            options("unknown-agent", "cli2key", "fixture"),
            &mut Vec::new(),
        )
        .expect_err("unknown agent must be rejected");
        assert!(
            unknown
                .to_string()
                .contains("unknown agent `unknown-agent`")
        );

        let unknown = setup(
            options("opencode", "unknown-provider", "fixture"),
            &mut Vec::new(),
        )
        .expect_err("unknown provider must be rejected");
        assert!(
            unknown
                .to_string()
                .contains("unknown provider `unknown-provider`")
        );

        let incompatible = setup(options("hermes", "gemini", "fixture"), &mut Vec::new())
            .expect_err("Gemini cannot satisfy Hermes endpoint contract");
        assert!(incompatible.to_string().contains("no compatible endpoint"));
    }
}
