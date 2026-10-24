use super::{CommandContext, CommandSpec, PreparationContext, StaticAdapter};
use crate::Error;
use crate::install::Recipe;
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "claude-code",
    "claude",
    Recipe::Npm {
        package: "@anthropic-ai/claude-code@2.1.301",
        executable: "claude",
        manifest: None,
    },
    &[crate::endpoint::MESSAGES_BOTH],
    command,
    prepare_adapter,
)
.with_replay_credential_vars(&["ANTHROPIC_AUTH_TOKEN"])
.with_state_environment_vars(&["CLAUDE_CONFIG_DIR"])
.with_unprivileged_execution()
.with_output_usage(output_usage)
.with_step_count(step_count);

fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    // Claude's final num_turns counts model round trips inside this CLI run.
    super::super::final_result_counter(stdout, "num_turns")
}

fn output_usage(
    stdout: &str,
    stderr: &str,
    input_includes_cache: bool,
) -> crate::measurements::TokenUsage {
    if let Ok(event) = serde_json::from_str::<serde_json::Value>(stdout.trim())
        && event.get("type").and_then(|v| v.as_str()) == Some("result")
        && let Some(value) = event.get("usage")
    {
        let mut usage = super::super::usage_fields(value);
        if !input_includes_cache {
            usage.input_tokens = usage.input_tokens.map(|n| {
                n.saturating_add(usage.cached_tokens.unwrap_or(0))
                    .saturating_add(
                        value
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0),
                    )
            });
        }
        return usage.with_derived_total();
    }
    crate::usage::usage_from_text(&format!("{stdout}\n{stderr}"))
}
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "claude".into(),
        args: vec![
            "-p".into(),
            "--bare".into(),
            "--no-session-persistence".into(),
            "--output-format".into(),
            "json".into(),
            "--model".into(),
            context.config.model.clone(),
            "--dangerously-skip-permissions".into(),
            context.prompt.into(),
        ],
        append_prompt: false,
    }
}

fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    // Keep Claude Code's user-level settings (which may pin a local provider
    // endpoint) out of a project-local benchmark.  The adapter owns this
    // isolated config root so tape and live runs use the endpoint prepared by
    // the runner rather than ambient host configuration.
    let config_root = context.runtime_dir.join("claude-code/config");
    std::fs::create_dir_all(&config_root).map_err(|error| {
        Error::Run(format!(
            "cannot create Claude Code project config directory {}: {error}",
            config_root.display()
        ))
    })?;
    context.command.env("CLAUDE_CONFIG_DIR", &config_root);
    let connection = context.connection;
    let key = connection.credential.as_deref().unwrap_or("asb-local");
    context
        .command
        .env(
            "ANTHROPIC_BASE_URL",
            connection.base_url.trim_end_matches("/v1"),
        )
        .env_remove("CLAUDE_CODE_USE_BEDROCK")
        .env_remove("CLAUDE_CODE_USE_VERTEX");
    match connection.authentication {
        crate::endpoint::Authentication::Bearer => {
            context
                .command
                .env("ANTHROPIC_API_KEY", "")
                .env("ANTHROPIC_AUTH_TOKEN", key);
        }
        crate::endpoint::Authentication::ApiKey => {
            context
                .command
                .env("ANTHROPIC_API_KEY", key)
                .env_remove("ANTHROPIC_AUTH_TOKEN");
        }
        crate::endpoint::Authentication::None => {
            context
                .command
                .env("ANTHROPIC_API_KEY", "")
                .env_remove("ANTHROPIC_AUTH_TOKEN");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentAdapter;

    #[test]
    fn adapter_reports_final_native_turns_including_error_results() {
        assert_eq!(
            ADAPTER.step_count(
                r#"{"type":"result","subtype":"error_max_turns","num_turns":3}"#,
                ""
            ),
            Some(3)
        );
        assert_eq!(
            ADAPTER.step_count(r#"{"type":"assistant","num_turns":3}"#, ""),
            None
        );
        assert_eq!(
            ADAPTER.step_count(r#"[{"type":"result","num_turns":3},{"type":"result"}]"#, ""),
            None
        );
    }

    #[test]
    fn unattended_execution_uses_the_owned_non_root_identity() {
        assert_eq!(
            ADAPTER.execution_identity(),
            crate::agents::ExecutionIdentity::Unprivileged
        );
        let config: crate::benchmark::BenchmarkConfig = toml::from_str(
            "agent='claude-code'\nworkload='fixture'\nprovider='fixture'\nmodel='fixture'\nworkspace='/work'\n",
        )
        .unwrap();
        let command = ADAPTER.command(&CommandContext {
            config: &config,
            endpoint: None,
            interface: None,
            prompt: "fixture prompt",
        });
        assert!(
            command
                .args
                .iter()
                .any(|argument| argument == "--dangerously-skip-permissions")
        );
    }
    #[test]
    fn uses_negotiated_authentication_without_conflicting_credentials() {
        let root = std::env::temp_dir().join(format!("asb-claude-auth-{}", std::process::id()));
        for auth in [
            crate::endpoint::Authentication::Bearer,
            crate::endpoint::Authentication::ApiKey,
        ] {
            let connection = crate::endpoint::Connection {
                interface: crate::endpoint::Interface::AnthropicMessages,
                base_url: "http://127.0.0.1:4242/api/v1".into(),
                credential: Some("fixture-key".into()),
                authentication: auth,
            };
            let mut command = std::process::Command::new("unused");
            prepare_adapter(&mut PreparationContext {
                command: &mut command,
                model: "fixture",
                connection: &connection,
                workspace: &root,
                runtime_dir: &root,
                run_dir: &root,
                tape: None,
            })
            .unwrap();
            let env = command
                .get_envs()
                .filter_map(|(key, value)| {
                    value.map(|value| {
                        (
                            key.to_string_lossy().into_owned(),
                            value.to_string_lossy().into_owned(),
                        )
                    })
                })
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:4242/api");
            match auth {
                crate::endpoint::Authentication::Bearer => {
                    assert_eq!(env["ANTHROPIC_API_KEY"], "");
                    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "fixture-key");
                }
                crate::endpoint::Authentication::ApiKey => {
                    assert_eq!(env["ANTHROPIC_API_KEY"], "fixture-key");
                    assert!(!env.contains_key("ANTHROPIC_AUTH_TOKEN"));
                }
                _ => unreachable!(),
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
