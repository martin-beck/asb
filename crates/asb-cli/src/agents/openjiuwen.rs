use super::{CommandContext, CommandSpec, PreparationContext, StaticAdapter};
use crate::{Error, install::Recipe};
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "openjiuwen",
    "openjiuwen",
    Recipe::Python {
        package: "openjiuwen[cli]==0.1.19.post2",
        executable: "openjiuwen",
    },
    command,
    prepare_adapter,
);
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "openjiuwen".into(),
        args: [
            "-v",
            "--model",
            context.config.model.as_str(),
            "--provider",
            "anthropic",
            "--api-base",
            context.endpoint.unwrap_or_default(),
            "run",
            "-f",
            "json",
            &format!(
                "{}\n\nDo not use todo/planning tools for this run. Execute the requested work directly and return a final response when complete.",
                context.prompt
            ),
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        append_prompt: false,
    }
}

#[cfg(test)]
mod tests {
    use super::command;
    use crate::agents::CommandContext;
    use crate::benchmark::BenchmarkConfig;

    #[test]
    fn enables_verbose_mode_for_deterministic_noninteractive_runs() {
        let config = BenchmarkConfig {
            agent: "openjiuwen".into(),
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
        let spec = command(&CommandContext {
            config: &config,
            endpoint: Some("http://127.0.0.1:3000/v1"),
            prompt: "repair",
        });
        assert_eq!(spec.args.first().map(String::as_str), Some("-v"));
        assert!(!spec.args.iter().any(|arg| arg == "-w"));
    }
}

fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    // OpenJiuwen's `-w` option hangs in non-interactive replay when pointed
    // at an isolated workspace. Its configuration loader also accepts the
    // equivalent environment setting; keep the CLI invocation on its stable
    // default path while still isolating every benchmark workspace.
    context
        .command
        .env("OPENJIUWEN_WORKSPACE", context.workspace);
    if context.provider.eq_ignore_ascii_case("cli2key") {
        if let Some((key, _)) = context.cli2key {
            context.command.env("OPENJIUWEN_API_KEY", key);
        }
    }
    Ok(())
}
