//! Repository-owned, versioned setup presets.
//!
//! Presets are deliberately compiled into the binary.  This keeps installed
//! ASB usable without a source checkout while retaining the reviewable TOML
//! documents under the repository `presets/` directory.

use crate::{Error, benchmark::RetentionConfig, config::SetupOptions};
use serde::Deserialize;

const HARNESSTAX: &str = include_str!("../../../presets/harnesstax.toml");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Preset {
    schema_version: u8,
    agents: Vec<String>,
    providers: Vec<String>,
    models: Vec<String>,
    workloads: Vec<Workload>,
    concurrency: usize,
    repeat: u64,
    #[serde(default)]
    measurements: Vec<String>,
    #[serde(default)]
    max_steps: Option<u64>,
    #[serde(default)]
    retention: RetentionConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Workload {
    id: String,
    trials: Vec<String>,
}

/// Execution defaults declared by a repository-owned preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SweepDefaults {
    pub(crate) repeat: u64,
}

/// Load one registered preset without consulting the network or project state.
pub(crate) fn load(name: &str) -> Result<SetupOptions, Error> {
    let preset = parse(name)?;
    Ok(SetupOptions {
        agent: preset.agents,
        workload: preset
            .workloads
            .iter()
            .map(|workload| workload.id.clone())
            .collect(),
        trial: preset
            .workloads
            .iter()
            .flat_map(|workload| {
                workload
                    .trials
                    .iter()
                    .map(move |trial| format!("{}={trial}", workload.id))
            })
            .collect(),
        trial_set: Vec::new(),
        provider: preset.providers,
        model: preset.models,
        measure: preset.measurements,
        concurrency: Some(preset.concurrency),
        workspace: None,
        output: None,
        retention: preset.retention,
    })
}

/// Load the run-time defaults without coupling them to generated TOMLs.
pub(crate) fn sweep_defaults(name: &str) -> Result<SweepDefaults, Error> {
    let preset = parse(name)?;
    Ok(SweepDefaults {
        repeat: preset.repeat,
    })
}

fn parse(name: &str) -> Result<Preset, Error> {
    if name.contains('/') || name.contains('\\') || name != "harnesstax.toml" {
        return Err(Error::Input(format!(
            "unknown preset `{name}`; available presets: harnesstax.toml"
        )));
    }
    let preset: Preset = toml::from_str(HARNESSTAX)
        .map_err(|error| Error::Input(format!("invalid bundled preset `{name}`: {error}")))?;
    if preset.schema_version != 1 {
        return Err(Error::Input(format!(
            "bundled preset `{name}` uses unsupported schema_version {}",
            preset.schema_version
        )));
    }
    if preset.agents.is_empty()
        || preset.providers.is_empty()
        || preset.models.is_empty()
        || preset.workloads.is_empty()
        || preset.repeat == 0
        || preset.concurrency == 0
        || preset
            .workloads
            .iter()
            .any(|workload| workload.id.is_empty() || workload.trials.is_empty())
    {
        return Err(Error::Input(format!(
            "bundled preset `{name}` has an empty required selection or zero repeat/concurrency"
        )));
    }
    // The method's step cap is retained as reproducibility metadata. It is
    // not forwarded automatically until every selected harness has a verified,
    // equivalent native turn-limit flag. Passing it now would make the preset
    // fail closed before any benchmark starts.
    let _ = preset.max_steps;
    Ok(preset)
}

#[cfg(test)]
mod tests {
    use super::{load, sweep_defaults};

    #[test]
    fn harnesstax_is_a_bounded_machine_readable_matrix() {
        let preset = load("harnesstax.toml").expect("preset loads");
        assert_eq!(preset.agent, ["claude-code", "codex", "pi"]);
        assert_eq!(preset.provider, ["openrouter"]);
        assert_eq!(preset.model, ["openrouter=deepseek/deepseek-v4.1-flash"]);
        assert_eq!(preset.workload, ["swe-bench-lite", "terminal-bench-2.0"]);
        assert_eq!(preset.trial.len(), 60);
        assert_eq!(preset.concurrency, Some(1));
        assert_eq!(preset.measure.len(), 12);
        assert!(preset.measure.contains(&"bpftrace:all".to_owned()));
        assert!(preset.retention.compact_completed_attempts);
        assert_eq!(
            sweep_defaults("harnesstax.toml").expect("defaults load"),
            super::SweepDefaults { repeat: 3 }
        );
    }

    #[test]
    fn preset_names_are_repository_owned() {
        assert!(load("../harnesstax.toml").is_err());
        assert!(load("unknown.toml").is_err());
    }
}
