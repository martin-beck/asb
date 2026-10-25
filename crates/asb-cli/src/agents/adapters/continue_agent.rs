use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, PreparationContext,
    StaticAdapter,
};
use crate::{Error, install::Recipe};
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "continue",
    "cn",
    Recipe::Npm {
        package: "@continuedev/cli@1.5.47",
        executable: "cn",
        manifest: None,
    },
    &[crate::endpoint::CHAT, crate::endpoint::MESSAGES_KEY],
    command,
    prepare_adapter,
)
.with_replay_credential_vars(&["CONTINUE_API_KEY"])
.with_state_environment_vars(&["CONTINUE_CONFIG", "CONTINUE_GLOBAL_DIR"])
.with_collection(collect);
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "cn".into(),
        args: vec![
            "-p".into(),
            context.prompt.into(),
            "--model".into(),
            context.config.model.clone(),
            "--format".into(),
            "json".into(),
            "--auto".into(),
        ],
        prompt_delivery: super::PromptDelivery::Embedded,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let directory = context.run_dir.join("continue/config");
    let provider = match connection.interface {
        crate::endpoint::Interface::AnthropicMessages => "anthropic",
        crate::endpoint::Interface::OpenAiChat => "openai",
        _ => return Err(context.unsupported_endpoint("continue")),
    };
    let key = connection.credential.as_deref().unwrap_or("asb-local");
    let url = &connection.base_url;
    configure(
        &directory,
        provider,
        context.model,
        url,
        "ASB_AGENT_API_KEY",
    )?;
    context
        .command
        .env("CONTINUE_CONFIG", directory.join("config.yaml"))
        .env("CONTINUE_GLOBAL_DIR", &directory)
        .env("CONTINUE_API_KEY", key)
        .env("ASB_AGENT_API_KEY", key)
        .env("CONTINUE_API_BASE", url)
        .env("CONTINUE_MODEL", context.model);
    Ok(())
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let sessions = context.run_dir.join("continue/config/sessions");
    let Some(value) = super::super::single_usage_document(&sessions, |path| {
        path.extension().is_some_and(|s| s == "json")
            && path.file_name().is_some_and(|s| s != "sessions.json")
    }) else {
        return AgentArtifacts::default();
    };
    AgentArtifacts {
        token_usage: Some(session_usage(
            &value,
            context.interface == Some(crate::endpoint::Interface::AnthropicMessages)
                && !context.anthropic_input_includes_cache,
        )),
        steps: None,
        final_response: None,
        ..Default::default()
    }
}

fn session_usage(
    value: &serde_json::Value,
    add_anthropic_cache: bool,
) -> crate::measurements::TokenUsage {
    let Some(history) = value.get("history").and_then(|v| v.as_array()) else {
        return Default::default();
    };
    let mut sum = crate::measurements::TokenUsage::default();
    let mut seen = false;
    let mut cache_complete = true;
    for item in history {
        let Some(message) = item.get("message") else {
            return Default::default();
        };
        if message.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        // Continue can put estimates in its aggregate session usage. Only
        // per-message usage exists when its upstream returned usage metadata.
        let Some(usage) = message.get("usage") else {
            return Default::default();
        };
        let mut parsed = super::super::usage_fields(usage);
        parsed.cached_tokens = usage
            .pointer("/prompt_tokens_details/cache_read_tokens")
            .or_else(|| usage.pointer("/prompt_tokens_details/cached_tokens"))
            .and_then(|v| v.as_u64())
            .or(parsed.cached_tokens);
        if add_anthropic_cache {
            let write = usage
                .pointer("/prompt_tokens_details/cache_write_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            parsed.input_tokens = parsed.input_tokens.map(|v| {
                v.saturating_add(parsed.cached_tokens.unwrap_or(0))
                    .saturating_add(write)
            });
        }
        if parsed.input_tokens.is_none() || parsed.output_tokens.is_none() {
            return Default::default();
        }
        cache_complete &= parsed.cached_tokens.is_some();
        sum = sum.add(parsed);
        seen = true;
    }
    if !seen {
        return Default::default();
    }
    if !cache_complete {
        sum.cached_tokens = None;
    }
    sum.with_derived_total()
}

fn configure(
    directory: &std::path::Path,
    provider: &str,
    model: &str,
    base_url: &str,
    api_key_env: &str,
) -> Result<(), Error> {
    std::fs::create_dir_all(directory).map_err(|error| {
        Error::Run(format!(
            "cannot create Continue provider configuration directory: {error}"
        ))
    })?;
    let model = serde_json::to_string(model)
        .map_err(|error| Error::Run(format!("cannot render Continue model: {error}")))?;
    let base_url = serde_json::to_string(base_url)
        .map_err(|error| Error::Run(format!("cannot render Continue endpoint: {error}")))?;
    let configuration = format!(
        "name: ASB provider\nversion: 1.0.0\nmodels:\n  - name: {model}\n    provider: {provider}\n    model: {model}\n    apiBase: {base_url}\n    apiKey: ${{{{ secrets.//{api_key_env} }}}}\n"
    );
    std::fs::write(directory.join("config.yaml"), configuration).map_err(|error| {
        Error::Run(format!(
            "cannot write Continue provider configuration: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_provider_reported_per_message_usage_not_estimated_session_total() {
        let document = serde_json::json!({"usage":{"promptTokens":9999,"completionTokens":999},"history":[
            {"message":{"role":"user"}},
            {"message":{"role":"assistant","usage":{"prompt_tokens":10,"completion_tokens":3,"prompt_tokens_details":{"cache_read_tokens":4}}}},
            {"message":{"role":"assistant","usage":{"prompt_tokens":11,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":5}}}}
        ]});
        let result = session_usage(&document, false);
        assert_eq!(
            (
                result.input_tokens,
                result.output_tokens,
                result.cached_tokens,
                result.total_tokens
            ),
            (Some(21), Some(5), Some(9), Some(26))
        );
        let incomplete = serde_json::json!({"usage":{"promptTokens":9999,"completionTokens":999},"history":[{"message":{"role":"assistant","usage":{"prompt_tokens":10,"completion_tokens":3}}},{"message":{"role":"assistant"}}]});
        assert_eq!(session_usage(&incomplete, false).total_tokens, None);
    }

    #[test]
    fn anthropic_separate_cache_is_included_only_when_needed() {
        let doc = serde_json::json!({"history":[{"message":{"role":"assistant","usage":{"prompt_tokens":10,"completion_tokens":3,"prompt_tokens_details":{"cache_read_tokens":4,"cache_write_tokens":2}}}}]});
        assert_eq!(session_usage(&doc, true).total_tokens, Some(19));
        assert_eq!(session_usage(&doc, false).total_tokens, Some(13));
    }

    #[test]
    fn refuses_ambiguous_or_missing_run_sessions() {
        let root = std::env::temp_dir().join(format!("asb-continue-token-{}", std::process::id()));
        let directory = root.join("continue/config/sessions");
        std::fs::create_dir_all(&directory).unwrap();
        let doc = r#"{"history":[{"message":{"role":"assistant","usage":{"prompt_tokens":10,"completion_tokens":3}}}]}"#;
        std::fs::write(directory.join("first.json"), doc).unwrap();
        std::fs::write(directory.join("sessions.json"), "[]").unwrap();
        let context = CollectionContext {
            frozen: &Default::default(),
            run_dir: &root,
            interface: None,
            anthropic_input_includes_cache: false,
        };
        assert_eq!(
            collect(&context).token_usage.unwrap().total_tokens,
            Some(13)
        );
        std::fs::write(directory.join("second.json"), doc).unwrap();
        assert!(collect(&context).token_usage.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_configuration_is_owned_by_the_continue_adapter() {
        let directory =
            std::env::temp_dir().join(format!("asb-continue-config-{}", std::process::id()));
        configure(
            &directory,
            "anthropic",
            "test-model",
            "http://127.0.0.1:3456/v1",
            "CLI2KEY_API_KEY",
        )
        .expect("configuration succeeds");
        let text =
            std::fs::read_to_string(directory.join("config.yaml")).expect("configuration exists");
        assert!(text.contains("test-model"));
        assert!(text.contains("CLI2KEY_API_KEY"));
        let _ = std::fs::remove_dir_all(directory);
    }
}
