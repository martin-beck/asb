use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, PreparationContext,
    StaticAdapter,
};
use crate::{Error, install::Recipe};
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "openhands",
    "openhands",
    Recipe::Python {
        package: "openhands==1.16.0",
        dependencies: &[],
        executable: "openhands",
    },
    &[crate::endpoint::CHAT, crate::endpoint::MESSAGES_KEY],
    command,
    prepare_adapter,
)
.with_replay_credential_vars(&["LLM_API_KEY"])
.with_collection(collect);
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "openhands".into(),
        args: vec![
            "--headless".into(),
            "--json".into(),
            "--yolo".into(),
            "--override-with-envs".into(),
            "--task".into(),
            context.prompt.into(),
        ],
        prompt_delivery: super::PromptDelivery::Embedded,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let key = connection.credential.as_deref().unwrap_or("asb-local");
    let (prefix, url) = match connection.interface {
        crate::endpoint::Interface::AnthropicMessages => {
            ("anthropic", connection.base_url.trim_end_matches("/v1"))
        }
        crate::endpoint::Interface::OpenAiChat => ("openai", connection.base_url.as_str()),
        _ => return Err(context.unsupported_endpoint("openhands")),
    };
    context
        .command
        .env("LLM_API_KEY", key)
        .env("LLM_BASE_URL", url)
        .env("LLM_MODEL", format!("{prefix}/{}", context.model))
        .env(
            "OPENHANDS_CONVERSATIONS_DIR",
            context.run_dir.join("openhands/conversations"),
        )
        .env("OPENHANDS_SUPPRESS_BANNER", "1");
    Ok(())
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let directory = context.run_dir.join("openhands/conversations");
    let mut state = None;
    let Ok(entries) = std::fs::read_dir(directory) else {
        return AgentArtifacts::default();
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return AgentArtifacts::default();
        };
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if state.is_some() {
            return AgentArtifacts::default();
        }
        state = Some(entry.path().join("base_state.json"));
    }
    let Some(value) = state.as_deref().and_then(super::super::read_usage_document) else {
        return AgentArtifacts::default();
    };
    AgentArtifacts {
        token_usage: Some(conversation_usage(
            &value,
            context.interface == Some(crate::endpoint::Interface::AnthropicMessages)
                && context.anthropic_input_includes_cache,
        )),
        steps: None,
        final_response: None,
        ..Default::default()
    }
}

fn conversation_usage(
    value: &serde_json::Value,
    subtract_inclusive_anthropic_cache: bool,
) -> crate::measurements::TokenUsage {
    let Some(metrics) = value
        .pointer("/stats/usage_to_metrics")
        .and_then(|v| v.as_object())
    else {
        return Default::default();
    };
    let mut sum = crate::measurements::TokenUsage::default();
    let mut seen = false;
    let mut cache_complete = true;
    for metrics in metrics.values() {
        let Some(calls) = metrics.get("token_usages").and_then(|v| v.as_array()) else {
            return Default::default();
        };
        if let Some(latencies) = metrics.get("response_latencies").and_then(|v| v.as_array())
            && latencies.len() != calls.len()
        {
            return Default::default();
        }
        for call in calls {
            let mut parsed = super::super::usage_fields(call);
            // LiteLLM adds cache reads and writes to Anthropic wire input,
            // even for compatible providers whose wire input already includes them.
            if subtract_inclusive_anthropic_cache {
                let write = call
                    .get("cache_write_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                parsed.input_tokens = parsed.input_tokens.and_then(|v| {
                    v.checked_sub(parsed.cached_tokens.unwrap_or(0).saturating_add(write))
                });
            }
            if parsed.input_tokens.is_none() || parsed.output_tokens.is_none() {
                return Default::default();
            }
            cache_complete &= parsed.cached_tokens.is_some();
            sum = sum.add(parsed);
            seen = true;
        }
    }
    if !seen {
        return Default::default();
    }
    if !cache_complete {
        sum.cached_tokens = None;
    }
    sum.with_derived_total()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_each_model_call_without_double_counting_accumulated_stats() {
        let doc = serde_json::json!({"stats":{"usage_to_metrics":{
            "agent":{"accumulated_token_usage":{"prompt_tokens":999,"completion_tokens":999},"response_latencies":[{},{}],"token_usages":[{"prompt_tokens":10,"completion_tokens":3,"cache_read_tokens":4},{"prompt_tokens":11,"completion_tokens":2,"cache_read_tokens":5}]},
            "condenser":{"response_latencies":[{}],"token_usages":[{"prompt_tokens":5,"completion_tokens":1,"cache_read_tokens":0}]}
        }}});
        let usage = conversation_usage(&doc, false);
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cached_tokens,
                usage.total_tokens
            ),
            (Some(26), Some(6), Some(9), Some(32))
        );
    }

    #[test]
    fn incomplete_metrics_and_empty_initial_stats_are_unknown() {
        let doc = serde_json::json!({"stats":{"usage_to_metrics":{"agent":{"response_latencies":[{},{}],"token_usages":[{"prompt_tokens":10,"completion_tokens":3}]}}}});
        assert_eq!(conversation_usage(&doc, false).total_tokens, None);
        let empty = serde_json::json!({"stats":{"usage_to_metrics":{"agent":{"response_latencies":[],"token_usages":[]}}}});
        assert_eq!(conversation_usage(&empty, false).total_tokens, None);
    }

    #[test]
    fn inclusive_anthropic_cache_is_not_added_twice() {
        let doc = serde_json::json!({"stats":{"usage_to_metrics":{"agent":{"response_latencies":[{}],"token_usages":[{"prompt_tokens":16,"completion_tokens":3,"cache_read_tokens":4,"cache_write_tokens":2}]}}}});
        assert_eq!(conversation_usage(&doc, false).total_tokens, Some(19));
        assert_eq!(conversation_usage(&doc, true).total_tokens, Some(13));
    }

    #[test]
    fn refuses_ambiguous_or_missing_run_conversations() {
        let root = std::env::temp_dir().join(format!("asb-openhands-token-{}", std::process::id()));
        let conversations = root.join("openhands/conversations");
        std::fs::create_dir_all(conversations.join("first")).unwrap();
        let doc = r#"{"stats":{"usage_to_metrics":{"agent":{"response_latencies":[{}],"token_usages":[{"prompt_tokens":10,"completion_tokens":3,"cache_read_tokens":0}]}}}}"#;
        std::fs::write(conversations.join("first/base_state.json"), doc).unwrap();
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
        std::fs::create_dir_all(conversations.join("second")).unwrap();
        assert!(collect(&context).token_usage.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
