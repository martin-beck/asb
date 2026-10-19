use super::{CommandContext, CommandSpec, PreparationContext, StaticAdapter};
use crate::{Error, install::Recipe};
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "opencode",
    "opencode",
    Recipe::Npm {
        package: "opencode-ai@1.19.0",
        executable: "opencode",
    },
    &[
        crate::endpoint::CHAT,
        crate::endpoint::MESSAGES_KEY,
        crate::endpoint::RESPONSES,
    ],
    command,
    prepare_adapter,
);
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
        append_prompt: true,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let npm = match connection.interface {
        crate::endpoint::Interface::AnthropicMessages => "@ai-sdk/anthropic",
        crate::endpoint::Interface::OpenAiChat => "@ai-sdk/openai-compatible",
        crate::endpoint::Interface::OpenAiResponses => "@ai-sdk/openai",
        _ => unreachable!("unsupported OpenCode endpoint"),
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
}
