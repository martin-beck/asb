use super::{CommandContext, CommandSpec, PreparationContext, StaticAdapter};
use crate::{Error, install::Recipe};
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "goose",
    "goose",
    Recipe::Goose,
    &[crate::endpoint::CHAT, crate::endpoint::MESSAGES_KEY],
    command,
    prepare_adapter,
)
.with_replay_credential_vars(&["ANTHROPIC_AUTH_TOKEN"]);
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "goose".into(),
        args: vec![
            "run".into(),
            "--no-session".into(),
            "--max-turns".into(),
            // A single turn cannot complete even the bundled fixture: Goose
            // needs one turn to inspect and another to edit/test. Keep the
            // bound finite while allowing a normal agent trajectory.
            "16".into(),
            "--output-format".into(),
            "json".into(),
            "-t".into(),
            context.prompt.into(),
        ],
        append_prompt: false,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let key = connection.credential.as_deref().unwrap_or("asb-local");
    match connection.interface {
        crate::endpoint::Interface::AnthropicMessages => {
            context
                .command
                .env("GOOSE_PROVIDER", "anthropic")
                .env("GOOSE_MODEL", context.model)
                .env(
                    "ANTHROPIC_HOST",
                    connection.base_url.trim_end_matches("/v1"),
                )
                .env("ANTHROPIC_API_KEY", key)
                .env("ANTHROPIC_AUTH_TOKEN", key);
        }
        crate::endpoint::Interface::OpenAiChat => {
            context
                .command
                .env("GOOSE_PROVIDER", "openai")
                .env("GOOSE_MODEL", context.model)
                .env("OPENAI_HOST", connection.base_url.trim_end_matches("/v1"))
                .env("OPENAI_BASE_PATH", "/v1/chat/completions")
                .env("OPENAI_API_KEY", key);
        }
        _ => unreachable!("Goose advertises only chat and messages interfaces"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;

    fn environment(command: &Command) -> HashMap<String, String> {
        command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn openrouter_replay_uses_tape_and_dummy_credential() {
        let mut command = Command::new("goose");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("asb-replay".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        prepare_adapter(&mut PreparationContext {
            command: &mut command,
            model: "test-model",
            connection: &connection,
            runtime_dir: Path::new("."),
        })
        .unwrap();
        let env = environment(&command);
        assert_eq!(env["GOOSE_PROVIDER"], "openai");
        assert_eq!(env["OPENAI_HOST"], "http://127.0.0.1:4321");
        assert_eq!(env["OPENAI_BASE_PATH"], "/v1/chat/completions");
        assert_eq!(env["OPENAI_API_KEY"], "asb-replay");
    }

    #[test]
    fn cli2key_tape_endpoint_wins_over_bridge_endpoint() {
        for mode in [crate::tape::Mode::Record, crate::tape::Mode::Replay] {
            let mut command = Command::new("goose");
            let connection = crate::endpoint::negotiate(
                "fixture-agent",
                &[crate::endpoint::MESSAGES_KEY],
                "cli2key",
                "test-model",
                Some(("fixture-key", "http://127.0.0.1:9000/v1")),
                Some((mode, "http://127.0.0.1:4321/v1")),
            )
            .unwrap();
            prepare_adapter(&mut PreparationContext {
                command: &mut command,
                model: "test-model",
                connection: &connection,
                runtime_dir: Path::new("."),
            })
            .unwrap();
            let env = environment(&command);
            assert_eq!(env["GOOSE_PROVIDER"], "anthropic");
            assert_eq!(env["ANTHROPIC_HOST"], "http://127.0.0.1:4321");
            assert_eq!(
                env["ANTHROPIC_API_KEY"],
                if mode == crate::tape::Mode::Replay {
                    "asb-replay"
                } else {
                    "fixture-key"
                }
            );
        }
    }

    #[test]
    fn bounded_turn_budget_allows_inspect_edit_and_test() {
        let config = crate::benchmark::BenchmarkConfig {
            agent: "goose".into(),
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
            interface: Some(crate::endpoint::Interface::OpenAiChat),
            prompt: "repair",
        });
        assert_eq!(
            spec.args
                .windows(2)
                .find(|pair| pair[0] == "--max-turns")
                .map(|pair| pair[1].as_str()),
            Some("16")
        );
    }
}
