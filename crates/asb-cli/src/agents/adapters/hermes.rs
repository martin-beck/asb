//! Hermes Agent adapter.
//!
//! Hermes exposes a script-friendly one-shot mode (`-z`) which prints only
//! the final answer and can write a bounded JSON usage ledger.  Keeping the
//! usage ledger in the run directory lets ASB report provider usage without
//! parsing prose or retaining the agent transcript.

use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, PreparationContext,
    StaticAdapter,
};
use crate::{Error, install::Recipe};

pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "hermes",
    "hermes",
    Recipe::Python {
        package: "hermes-agent==0.19.0",
        dependencies: &[],
        executable: "hermes",
    },
    &[crate::endpoint::RESPONSES, crate::endpoint::MESSAGES_KEY],
    command,
    prepare_adapter,
)
.with_collection(collect)
.with_artifact_specs(&[super::super::ArtifactSpec {
    namespace: "hermes",
    filename: "usage.json",
    format: super::super::ArtifactFormat::Json,
}])
.with_state_environment_vars(&["HERMES_HOME"])
.with_final_response(final_response);

fn command(context: &CommandContext<'_>) -> CommandSpec {
    // Provider-specific aliases are selected by the provider layer. This
    // keeps Hermes independent of provider identifiers while ensuring bridge
    // endpoints do not fall back to Hermes' generic chat transport.
    let provider =
        crate::providers::agent_provider_name(&context.config.provider, context.interface);
    CommandSpec {
        program: "hermes".into(),
        args: vec![
            "-z".into(),
            context.prompt.into(),
            "--provider".into(),
            provider.into(),
            "--model".into(),
            context.config.model.clone(),
        ],
        prompt_delivery: super::PromptDelivery::Embedded,
    }
}

fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let key = connection.credential.as_deref().unwrap_or("asb-local");
    let hermes_home = context.runtime_dir.join("hermes");
    std::fs::create_dir_all(&hermes_home).map_err(|error| {
        Error::Run(format!(
            "cannot create Hermes project runtime directory {}: {error}",
            hermes_home.display()
        ))
    })?;
    {
        let (api_mode, key_env) = match connection.interface {
            crate::endpoint::Interface::OpenAiResponses => ("codex_responses", "OPENAI_API_KEY"),
            crate::endpoint::Interface::AnthropicMessages => {
                ("anthropic_messages", "ANTHROPIC_API_KEY")
            }
            _ => {
                return Err(Error::Input(
                    "Hermes requires a negotiated OpenAI Responses or Anthropic Messages endpoint"
                        .into(),
                ));
            }
        };
        let base_url = if api_mode == "anthropic_messages" {
            connection.base_url.trim_end_matches("/v1")
        } else {
            connection.base_url.as_str()
        };
        let config = format!(
            "providers:\n  asb-cli2key:\n    base_url: {}\n    api_mode: {}\n    key_env: {}\n",
            yaml_quote(base_url),
            api_mode,
            key_env
        );
        std::fs::write(hermes_home.join("config.yaml"), config).map_err(|error| {
            Error::Run(format!(
                "cannot write Hermes cli2key provider configuration: {error}"
            ))
        })?;
    }
    let usage_file = context.run_dir.join("hermes/usage.json");
    if let Some(parent) = usage_file.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::Run(format!(
                "cannot create Hermes usage directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    context
        .command
        .env("HERMES_HOME", &hermes_home)
        .arg("--usage-file")
        .arg(&usage_file);
    match connection.interface {
        crate::endpoint::Interface::OpenAiResponses => {
            context
                .command
                .env("OPENAI_API_KEY", key)
                .env("OPENAI_BASE_URL", &connection.base_url)
                .env_remove("OPENROUTER_API_KEY")
                .env_remove("ANTHROPIC_API_KEY")
                .env_remove("ANTHROPIC_AUTH_TOKEN");
        }
        crate::endpoint::Interface::AnthropicMessages => {
            context
                .command
                .env("ANTHROPIC_API_KEY", key)
                .env(
                    "ANTHROPIC_BASE_URL",
                    connection.base_url.trim_end_matches("/v1"),
                )
                .env_remove("OPENAI_API_KEY")
                .env_remove("OPENROUTER_API_KEY")
                .env_remove("ANTHROPIC_AUTH_TOKEN");
        }
        _ => {
            return Err(Error::Input(
                "Hermes supports only OpenAI Responses or Anthropic Messages provider connections"
                    .into(),
            ));
        }
    }
    Ok(())
}

fn yaml_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let Some(value) = context.frozen.document("usage.json") else {
        return AgentArtifacts::default();
    };
    AgentArtifacts {
        token_usage: Some(super::super::usage_fields(value).with_derived_total()),
        steps: value.get("api_calls").and_then(serde_json::Value::as_u64),
        final_response: None,
        ..Default::default()
    }
}

fn final_response(output: &str, _workspace: &std::path::Path) -> Option<String> {
    let response = output.trim();
    (!response.is_empty()).then(|| response.to_owned())
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
    fn command_uses_noninteractive_mode_and_selected_wire_provider() {
        let config = crate::benchmark::BenchmarkConfig {
            planning: None,
            agent: "hermes".into(),
            provider: "openrouter".into(),
            model: "openai/gpt-4o-mini".into(),
            workload: "adapter-smoke".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "./asb-workspace".into(),
            log_file: "benchmark.log".into(),
            measurements: None,
            mcp_servers: Vec::new(),
        };
        let spec = command(&CommandContext {
            config: &config,
            endpoint: Some("http://127.0.0.1:1/v1"),
            interface: Some(crate::endpoint::Interface::OpenAiResponses),
            prompt: "solve the fixture",
        });
        assert_eq!(spec.program, "hermes");
        assert_eq!(spec.args[0], "-z");
        assert!(spec.args.contains(&"openai-api".into()));
        assert_eq!(
            spec.prompt_delivery,
            crate::agents::PromptDelivery::Embedded
        );
    }

    #[test]
    fn preparation_is_project_local_and_routes_openai_without_ambient_credentials() {
        let root = std::env::temp_dir().join(format!("asb-hermes-{}", std::process::id()));
        let run = root.join("run");
        let mut command = Command::new("hermes");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiResponses,
            base_url: "http://127.0.0.1:4242/v1".into(),
            credential: Some("fixture-key".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        prepare_adapter(&mut PreparationContext {
            command: &mut command,
            model: "fixture",
            connection: &connection,
            workspace: &root,
            runtime_dir: &root,
            run_dir: &run,
            mcp_servers: &[],
            tape: None,
        })
        .unwrap();
        let env = environment(&command);
        assert_eq!(env["OPENAI_API_KEY"], "fixture-key");
        assert_eq!(env["OPENAI_BASE_URL"], "http://127.0.0.1:4242/v1");
        assert!(env["HERMES_HOME"].contains("hermes"));
        let provider_config = std::fs::read_to_string(root.join("hermes/config.yaml")).unwrap();
        assert!(provider_config.contains("api_mode: codex_responses"));
        assert!(provider_config.contains("base_url: 'http://127.0.0.1:4242/v1'"));
        assert!(command.get_args().any(|arg| arg == "--usage-file"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cli2key_command_uses_named_responses_provider() {
        let config = crate::benchmark::BenchmarkConfig {
            planning: None,
            agent: "hermes".into(),
            provider: "cli2key".into(),
            model: "gpt-6.1-sol".into(),
            workload: "terminal-bench".into(),
            concurrency: 2,
            trial_set: None,
            trial: None,
            workspace: "./asb-workspace".into(),
            log_file: "benchmark.log".into(),
            measurements: None,
            mcp_servers: Vec::new(),
        };
        let spec = command(&CommandContext {
            config: &config,
            endpoint: Some("http://127.0.0.1:1/v1"),
            interface: Some(crate::endpoint::Interface::OpenAiResponses),
            prompt: "solve the fixture",
        });
        assert!(spec.args.contains(&"asb-cli2key".into()));
        assert!(!spec.args.contains(&"openai-api".into()));
    }

    #[test]
    fn usage_ledger_is_collected_without_parsing_response_text() {
        let root = std::env::temp_dir().join(format!("asb-hermes-usage-{}", std::process::id()));
        let path = root.join("hermes/usage.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"input_tokens":11,"output_tokens":7,"cache_read_tokens":3,"total_tokens":21,"api_calls":4}"#,
        )
        .unwrap();
        let artifacts = collect(&CollectionContext {
            frozen: &super::super::super::FrozenArtifacts::for_test(
                "usage.json",
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap(),
            ),
            run_dir: &root,
            interface: Some(crate::endpoint::Interface::OpenAiResponses),
            anthropic_input_includes_cache: false,
        });
        let usage = artifacts.token_usage.unwrap();
        assert_eq!(usage.input_tokens, Some(11));
        assert_eq!(usage.output_tokens, Some(7));
        assert_eq!(usage.cached_tokens, Some(3));
        assert_eq!(usage.total_tokens, Some(18));
        assert_eq!(artifacts.steps, Some(4));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn final_response_accepts_hermes_oneshot_output_only() {
        assert_eq!(
            final_response("  final answer\n", Path::new(".")),
            Some("final answer".into())
        );
        assert_eq!(final_response("\n", Path::new(".")), None);
    }
}
