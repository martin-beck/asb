//! The setup command and its small file format.

use super::{Error, io_error};
use crate::benchmark::{BenchmarkConfig, MeasurementConfig};
use crate::measurements;
use crate::project;
use crate::providers;
use crate::suggestions;
use std::io::Write;

/// Parse setup flags and write a TOML benchmark configuration.
pub fn setup(args: &[String], output: &mut dyn Write) -> Result<(), Error> {
    let mut agent = None;
    let mut workload = None;
    let mut provider = None;
    let mut model = None;
    let mut workspace = None;
    let mut destination = None;
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if let Some(name) = flag.strip_prefix('-').filter(|name| {
            !name.starts_with('-')
                && matches!(
                    *name,
                    "agent" | "workload" | "provider" | "model" | "workspace" | "output"
                )
        }) {
            return Err(Error::Input(format!(
                "setup option `{flag}` is invalid; use `--{name}` with two dashes"
            )));
        }
        let value = args
            .get(index + 1)
            .ok_or(Error::Usage("setup options require values"))?;
        match flag {
            "--agent" => agent = Some(value.clone()),
            "--workload" => workload = Some(value.clone()),
            "--provider" => provider = Some(value.clone()),
            "--model" => model = Some(value.clone()),
            "--workspace" => workspace = Some(value.clone()),
            "--output" => destination = Some(value.clone()),
            _ => {
                let options = [
                    "--agent",
                    "--workload",
                    "--provider",
                    "--model",
                    "--workspace",
                    "--output",
                ];
                return Err(Error::Input(format!(
                    "{}",
                    suggestions::unknown("setup option", flag, &options)
                )));
            }
        }
        index += 2;
    }
    let (_, project_config) = project::load(std::path::Path::new("."))?;
    let agent = required(agent)?;
    if let Some(candidate) = suggestions::closest(
        &agent,
        &[
            "codex",
            "pi",
            "opencode",
            "workbuddy",
            "gemini",
            "qwen-code",
            "aider",
            "mini-swe",
            "openhands",
            "openjiuwen",
        ],
    ) {
        return Err(Error::Input(format!(
            "unknown agent `{agent}`; did you mean `{candidate}`?"
        )));
    }
    let provider = required(provider)?;
    if let Some(candidate) = suggestions::closest(&provider, providers::names()) {
        return Err(Error::Input(format!(
            "unknown provider `{provider}`; did you mean `{candidate}`?"
        )));
    }
    let model = required(model)?;
    if let Some(candidates) = providers::known_models(&provider) {
        if !candidates.iter().any(|candidate| *candidate == model) {
            if let Some(candidate) = suggestions::closest(&model, candidates) {
                return Err(Error::Input(format!(
                    "model `{model}` for provider `{provider}` is not tool-capable; did you mean `{candidate}`?"
                )));
            }
            return Err(Error::Input(format!(
                "model `{model}` for provider `{provider}` is not in the tool-capable catalog; run `asb provider models {provider}`"
            )));
        }
    }
    let config = BenchmarkConfig {
        agent,
        workload: required(workload)?,
        provider,
        model,
        trial_set: None,
        trial: None,
        workspace: workspace.unwrap_or(project_config.workspace_dir),
        log_file: destination
            .as_deref()
            .map(default_log_name)
            .unwrap_or_else(|| "benchmark.log".into()),
        measurements: Some(MeasurementConfig {
            enabled: true,
            directory: "measurements".into(),
            collectors: measurements::available_collectors(),
        }),
    };
    let text = toml::to_string_pretty(&config)
        .map_err(|error| Error::Input(format!("cannot render configuration: {error}")))?;
    if let Some(path) = destination {
        std::fs::write(&path, text.as_bytes())
            .map_err(|error| Error::Io(format!("cannot write {path}: {error}")))?;
        writeln!(output, "Wrote {path}").map_err(io_error)?;
        writeln!(output, "{}", run_command_message(&path)).map_err(io_error)?;
    } else {
        output.write_all(text.as_bytes()).map_err(io_error)?;
    }
    Ok(())
}

fn default_log_name(destination: &str) -> String {
    std::path::Path::new(destination)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(|value| format!("{value}.log"))
        .unwrap_or_else(|| "benchmark.log".into())
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
    use super::{default_log_name, run_command_message, setup};

    #[test]
    fn setup_derives_log_name_from_configuration_stem() {
        assert_eq!(default_log_name("configs/foo.toml"), "foo.log");
        assert_eq!(default_log_name("benchmark"), "benchmark.log");
    }

    #[test]
    fn setup_output_explains_how_to_run_written_configuration() {
        assert_eq!(
            run_command_message("configs/foo.toml"),
            "Run it with: asb run configs/foo.toml"
        );
    }

    #[test]
    fn explains_single_dash_long_option_typo() {
        let error = setup(
            &[
                "--agent".into(),
                "opencode".into(),
                "-workload".into(),
                "bug-fix".into(),
            ],
            &mut Vec::new(),
        )
        .expect_err("single-dash option must be rejected");
        assert_eq!(
            error.to_string(),
            "setup option `-workload` is invalid; use `--workload` with two dashes"
        );
    }

    #[test]
    fn setup_suggests_close_agent_and_provider_names() {
        let agent_error = setup(
            &[
                "--agent".into(),
                "opencod".into(),
                "--workload".into(),
                "bug-fix".into(),
                "--provider".into(),
                "local".into(),
                "--model".into(),
                "test".into(),
            ],
            &mut Vec::new(),
        )
        .expect_err("agent typo rejected");
        assert!(agent_error.to_string().contains("did you mean `opencode`?"));
        let provider_error = setup(
            &[
                "--agent".into(),
                "opencode".into(),
                "--workload".into(),
                "bug-fix".into(),
                "--provider".into(),
                "openruter".into(),
                "--model".into(),
                "test".into(),
            ],
            &mut Vec::new(),
        )
        .expect_err("provider typo rejected");
        assert!(
            provider_error
                .to_string()
                .contains("did you mean `openrouter`?")
        );
        let model_error = setup(
            &[
                "--agent".into(),
                "opencode".into(),
                "--workload".into(),
                "bug-fix".into(),
                "--provider".into(),
                "cli2key".into(),
                "--model".into(),
                "gpt-6.1-sl".into(),
            ],
            &mut Vec::new(),
        )
        .expect_err("model typo rejected");
        assert!(
            model_error
                .to_string()
                .contains("did you mean `gpt-6.1-sol`?")
        );
    }
}
