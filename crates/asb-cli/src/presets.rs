//! Repository-owned, versioned setup presets.
//!
//! Presets are deliberately compiled into the binary.  This keeps installed
//! ASB usable without a source checkout while retaining the reviewable TOML
//! documents under the repository `presets/` directory.

use crate::{Error, config::SetupOptions};
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Workload {
    id: String,
    trials: Vec<String>,
}

/// Load one registered preset without consulting the network or project state.
pub(crate) fn load(name: &str) -> Result<SetupOptions, Error> {
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
    // `max_steps` is retained as experiment methodology in the preset.  It is
    // intentionally not translated to an adapter command here: ASB currently
    // has no uniform live step-cap protocol across these three independent
    // harnesses.  The separate max-step controller feature owns enforcement.
    let _ = preset.max_steps;
    let options = SetupOptions {
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
    };
    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::load;

    #[test]
    fn harnesstax_is_a_bounded_machine_readable_matrix() {
        let preset = load("harnesstax.toml").expect("preset loads");
        assert_eq!(preset.agent, ["claude-code", "codex", "pi"]);
        assert_eq!(preset.provider, ["openrouter"]);
        assert_eq!(preset.model, ["openrouter=deepseek/deepseek-v4.1-flash"]);
        assert_eq!(preset.workload, ["swe-bench-lite", "terminal-bench-2.0"]);
        assert_eq!(preset.trial.len(), 60);
        assert_eq!(preset.concurrency, Some(1));
    }

    #[test]
    fn preset_names_are_repository_owned() {
        assert!(load("../harnesstax.toml").is_err());
        assert!(load("unknown.toml").is_err());
    }
}
