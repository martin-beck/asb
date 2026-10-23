use super::{CommandContext, CommandSpec, PreparationContext, StaticAdapter};
use crate::{Error, install::Recipe};
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "cline",
    "cline",
    Recipe::Npm {
        package: "cline@3.0.72",
        executable: "cline",
        manifest: None,
    },
    &[crate::endpoint::CHAT, crate::endpoint::RESPONSES],
    command,
    prepare_adapter,
)
.with_replay_credential_vars(&["CLINE_API_KEY"])
.with_state_environment_vars(&[
    "CLINE_DIR",
    "CLINE_DATA_DIR",
    "CLINE_PROVIDER_SETTINGS_PATH",
    "CLINE_GLOBAL_SETTINGS_PATH",
])
.with_output_usage(output_usage)
.with_step_count(step_count);

fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    stdout
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event.get("type").and_then(|v| v.as_str()) == Some("run_result"))
        .and_then(|event| event.get("iterations").and_then(|v| v.as_u64()))
}

fn output_usage(stdout: &str, stderr: &str, _: bool) -> crate::measurements::TokenUsage {
    for event in stdout
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if event.get("type").and_then(|v| v.as_str()) == Some("run_result")
            && let Some(usage) = event.get("aggregateUsage").or_else(|| event.get("usage"))
        {
            return super::super::usage_fields(usage).with_derived_total();
        }
    }
    crate::usage::usage_from_text(&format!("{stdout}\n{stderr}"))
}
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "cline".into(),
        args: vec![
            "--json".into(),
            "--auto-approve".into(),
            "true".into(),
            "--provider".into(),
            client(context.interface).into(),
            "--model".into(),
            context.config.model.clone(),
            context.prompt.into(),
        ],
        append_prompt: false,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let directory = context.runtime_dir.join("cline/config");
    let connection = context.connection;
    let key = connection.credential.as_deref().unwrap_or("asb-local");
    let protocol = match connection.interface {
        crate::endpoint::Interface::OpenAiResponses => "openai-responses",
        crate::endpoint::Interface::OpenAiChat => "openai-chat-completions",
        _ => return Err(context.unsupported_endpoint("cline")),
    };
    configure(
        &directory,
        client(Some(connection.interface)),
        context.model,
        &connection.base_url,
        key,
        protocol,
    )?;
    context
        .command
        .env("OPENAI_API_KEY", key)
        .env("OPENAI_BASE_URL", &connection.base_url)
        .env("OPENAI_API_BASE", &connection.base_url)
        .env("OPENAI_MODEL", context.model)
        .env("CLINE_API_KEY", key);
    context
        .command
        .env("CLINE_DIR", &directory)
        .env("CLINE_DATA_DIR", directory.join("data"))
        .env(
            "CLINE_PROVIDER_SETTINGS_PATH",
            directory.join("data/settings/providers.json"),
        )
        .env(
            "CLINE_GLOBAL_SETTINGS_PATH",
            directory.join("data/settings/global-settings.json"),
        );
    Ok(())
}

fn client(interface: Option<crate::endpoint::Interface>) -> &'static str {
    match interface {
        Some(crate::endpoint::Interface::OpenAiChat) => "openai-compatible",
        _ => "openai-native",
    }
}

fn configure(
    directory: &std::path::Path,
    provider: &str,
    model: &str,
    base_url: &str,
    key: &str,
    protocol: &str,
) -> Result<(), Error> {
    let settings_dir = directory.join("data/settings");
    std::fs::create_dir_all(&settings_dir).map_err(|error| {
        Error::Run(format!(
            "cannot create Cline provider configuration directory: {error}"
        ))
    })?;
    let mut configuration = serde_json::json!({
        "version": 1, "lastUsedProvider": provider, "modes": {},
        "providers": { provider: { "settings": {
            "provider": provider, "model": model, "baseUrl": base_url, "apiKey": key
        }, "updatedAt": "1970-01-01T00:00:00.000Z", "tokenSource": "manual"}}
    });
    if protocol == "openai-responses" {
        configuration["providers"][provider]["settings"]["protocol"] = protocol.into();
    }
    let content = serde_json::to_vec_pretty(&configuration).map_err(|error| {
        Error::Run(format!(
            "cannot render Cline provider configuration: {error}"
        ))
    })?;
    std::fs::write(settings_dir.join("providers.json"), content).map_err(|error| {
        Error::Run(format!(
            "cannot write Cline provider configuration: {error}"
        ))
    })?;
    let global = serde_json::json!({
        "disabledTools": ["web_search", "image_generation"],
        "tools": {"web_search": {"enabled": false}, "image_generation": {"enabled": false}}
    });
    let global_content = serde_json::to_vec_pretty(&global)
        .map_err(|error| Error::Run(format!("cannot render Cline global settings: {error}")))?;
    std::fs::write(settings_dir.join("global-settings.json"), global_content)
        .map_err(|error| Error::Run(format!("cannot write Cline global settings: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_configuration_is_owned_by_the_cline_adapter() {
        let directory =
            std::env::temp_dir().join(format!("asb-cline-config-{}", std::process::id()));
        configure(
            &directory,
            "openai-native",
            "test-model",
            "http://127.0.0.1:3456/v1",
            "test-key",
            "openai-responses",
        )
        .expect("configuration succeeds");
        let text = std::fs::read_to_string(directory.join("data/settings/providers.json"))
            .expect("configuration exists");
        assert!(text.contains("test-model"));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn compatible_chat_settings_keep_the_selected_base_url() {
        let directory = std::env::temp_dir().join(format!("asb-cline-chat-{}", std::process::id()));
        configure(
            &directory,
            client(Some(crate::endpoint::Interface::OpenAiChat)),
            "fixture/model",
            "http://127.0.0.1:3456/v1",
            "fixture-key",
            "openai-chat-completions",
        )
        .unwrap();
        let settings: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.join("data/settings/providers.json")).unwrap(),
        )
        .unwrap();
        let entry = &settings["providers"]["openai-compatible"]["settings"];
        assert_eq!(entry["baseUrl"], "http://127.0.0.1:3456/v1");
        assert_eq!(entry["apiKey"], "fixture-key");
        assert!(entry.get("protocol").is_none());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
