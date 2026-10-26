use super::{CommandContext, CommandSpec, PreparationContext, PromptDelivery, StaticAdapter};
use crate::{Error, install::Recipe};

// OpenCode's Bedrock integration currently resolves two AWS SDK entries whose
// published registry metadata can outpace their tarballs. This repair belongs
// to the owning adapter, never to unrelated npm-installed tools or agents.
const NPM_MANIFEST: &str = r#"{
  "private": true,
  "overrides": {
    "@aws-sdk/client-bedrock-runtime": "3.1030.0",
    "@aws-sdk/credential-provider-http": "3.972.76"
  }
}
"#;

pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "opencode",
    "opencode",
    Recipe::Npm {
        package: "opencode-ai@1.19.0",
        executable: "opencode",
        manifest: Some(NPM_MANIFEST),
    },
    &[
        crate::endpoint::CHAT,
        crate::endpoint::MESSAGES_KEY,
        crate::endpoint::RESPONSES,
    ],
    command,
    prepare_adapter,
)
.with_output_usage(output_usage)
.with_step_count(step_count)
.with_deterministic_extension()
.with_state_environment_vars(&[
    "OPENCODE_CONFIG_CONTENT",
    "OPENCODE_CONFIG",
    "OPENCODE_CONFIG_DIR",
]);

/// OpenCode emits one JSONL `step_finish` event for every completed agent step.
/// A terminal answer may be a plain text event, so it must not be used as a
/// substitute for this lifecycle counter.
fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    let steps = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event.get("type").and_then(|value| value.as_str()) == Some("step_finish"))
        .count()
        .try_into()
        .ok()?;
    (steps > 0).then_some(steps)
}

fn output_usage(stdout: &str, stderr: &str, _: bool) -> crate::measurements::TokenUsage {
    let mut sum = crate::measurements::TokenUsage::default();
    let mut seen = false;
    let mut cache_complete = true;
    for event in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if event.get("type").and_then(|v| v.as_str()) != Some("step_finish") {
            continue;
        }
        let Some(tokens) = event.pointer("/part/tokens") else {
            return Default::default();
        };
        let number = |name: &str| tokens.get(name).and_then(|v| v.as_u64());
        let (Some(input), Some(output)) = (number("input"), number("output")) else {
            return Default::default();
        };
        let read = tokens.pointer("/cache/read").and_then(|v| v.as_u64());
        let write = tokens.pointer("/cache/write").and_then(|v| v.as_u64());
        cache_complete &= read.is_some();
        sum = sum.add(crate::measurements::TokenUsage {
            input_tokens: Some(
                input
                    .saturating_add(read.unwrap_or(0))
                    .saturating_add(write.unwrap_or(0)),
            ),
            output_tokens: Some(output),
            cached_tokens: read,
            ..Default::default()
        });
        seen = true;
    }
    if seen {
        if !cache_complete {
            sum.cached_tokens = None;
        }
        sum.with_derived_total()
    } else {
        crate::usage::usage_from_text(&format!("{stdout}\n{stderr}"))
    }
}
fn command(context: &CommandContext<'_>) -> CommandSpec {
    let model = format!("asb/{}", context.config.model);
    CommandSpec {
        program: "opencode".into(),
        args: vec![
            "run".into(),
            "--dir".into(),
            context.config.workspace.clone(),
            "--model".into(),
            model,
            "--format".into(),
            "json".into(),
        ],
        prompt_delivery: PromptDelivery::Argument,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let npm = match connection.interface {
        crate::endpoint::Interface::AnthropicMessages => "@ai-sdk/anthropic",
        crate::endpoint::Interface::OpenAiChat => "@ai-sdk/openai-compatible",
        crate::endpoint::Interface::OpenAiResponses => "@ai-sdk/openai",
        _ => return Err(context.unsupported_endpoint("opencode")),
    };
    context
        .command
        .env(
            "ASB_AGENT_API_KEY",
            connection.credential.as_deref().unwrap_or("asb-local"),
        )
        .env(
            "OPENCODE_CONFIG_CONTENT",
            config(context.model, &connection.base_url, npm)?,
        );
    // OpenCode's repository-search tool invokes ripgrep.  Fresh workload
    // materialization can give the same files a different directory-entry
    // order, which otherwise changes only the ordering of an identical tool
    // result and breaks strict replay history matching.  Keep this policy in
    // the owning adapter and use a private runtime file, never a host-wide
    // ripgrep configuration.
    let ripgrep_config = context.runtime_dir.join("opencode-ripgrep.conf");
    std::fs::write(&ripgrep_config, "--sort=path\n").map_err(|error| {
        Error::Run(format!(
            "cannot write deterministic OpenCode search configuration: {error}"
        ))
    })?;
    context.command.env("RIPGREP_CONFIG_PATH", ripgrep_config);
    Ok(())
}

fn config(model: &str, base_url: &str, npm: &str) -> Result<String, Error> {
    let mut models = serde_json::Map::new();
    models.insert(model.to_owned(), serde_json::json!({"name": model}));
    serde_json::to_string(&serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {"asb": {
            "npm": npm, "name": "ASB provider",
            "options": {"baseURL": base_url, "apiKey": "{env:ASB_AGENT_API_KEY}"},
            "models": models
        }}
    }))
    .map_err(|error| Error::Run(format!("cannot configure OpenCode provider: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_configuration_is_owned_by_the_opencode_adapter() {
        let config = config(
            "test-model",
            "http://127.0.0.1:3456/v1",
            "@ai-sdk/anthropic",
        )
        .expect("configuration succeeds");
        assert!(config.contains("test-model"));
        assert!(config.contains("ASB_AGENT_API_KEY"));
    }

    #[test]
    fn preparation_pins_private_path_sorted_searches() {
        let root = std::env::temp_dir().join(format!("asb-opencode-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        let mut command = std::process::Command::new("opencode");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:3456/v1".into(),
            credential: None,
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let mut context = PreparationContext {
            command: &mut command,
            model: "fixture",
            connection: &connection,
            workspace: &root,
            runtime_dir: &root.join("runtime"),
            run_dir: &root.join("run"),
            tape: None,
        };
        prepare_adapter(&mut context).unwrap();
        let config = root.join("runtime/opencode-ripgrep.conf");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "--sort=path\n");
        assert!(command.get_envs().any(|(name, value)| {
            name == "RIPGREP_CONFIG_PATH" && value.is_some_and(|value| value == config)
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn counts_completed_jsonl_steps_without_guessing_from_final_text() {
        let output = concat!(
            "{\"type\":\"step_start\"}\n",
            "{\"type\":\"step_finish\",\"part\":{\"tokens\":{\"input\":2,\"output\":1}}}\n",
            "{\"type\":\"text\",\"part\":{\"text\":\"done\"}}\n",
            "{\"type\":\"step_finish\",\"part\":{\"tokens\":{\"input\":3,\"output\":1}}}\n"
        );
        assert_eq!(step_count(output, ""), Some(2));
        assert_eq!(step_count("{\"type\":\"text\"}\n", ""), None);
        assert_eq!(step_count("not json\n", ""), None);
    }
}
