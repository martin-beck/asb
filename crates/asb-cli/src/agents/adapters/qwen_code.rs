use super::{CommandContext, CommandSpec, PreparationContext, PromptDelivery, StaticAdapter};
use crate::{Error, install::Recipe};
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "qwen-code",
    "qwen",
    Recipe::Npm {
        package: "@qwen-code/qwen-code@0.25.1",
        executable: "qwen",
        manifest: None,
    },
    &[crate::endpoint::CHAT, crate::endpoint::MESSAGES_KEY],
    command,
    prepare_adapter,
)
.with_output_usage(output_usage)
.with_step_count(step_count)
.with_final_response(final_response);

fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    super::super::final_result_counter(stdout, "num_turns")
}

fn output_usage(
    stdout: &str,
    _stderr: &str,
    _input_includes_cache: bool,
) -> crate::measurements::TokenUsage {
    fn result(value: &serde_json::Value) -> Option<crate::measurements::TokenUsage> {
        match value {
            serde_json::Value::Array(events) => events.iter().rev().find_map(result),
            serde_json::Value::Object(event)
                if event.get("type").and_then(|v| v.as_str()) == Some("result") =>
            {
                event
                    .get("usage")
                    .or_else(|| event.get("stats"))
                    .map(super::super::usage_fields)
            }
            _ => None,
        }
    }
    let usage = serde_json::from_str::<serde_json::Value>(stdout)
        .ok()
        .and_then(|v| result(&v))
        .or_else(|| {
            stdout.lines().rev().find_map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| result(&v))
            })
        })
        .unwrap_or_default();
    if usage.input_tokens == Some(0)
        && usage.output_tokens == Some(0)
        && usage.total_tokens.unwrap_or(0) == 0
    {
        return crate::measurements::TokenUsage {
            cached_tokens: usage.cached_tokens.filter(|n| *n > 0),
            ..Default::default()
        };
    }
    usage.with_derived_total()
}

fn final_response(output: &str, _workspace: &std::path::Path) -> Option<String> {
    super::super::structured_final_response_document(output)
}
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "qwen".into(),
        args: vec![
            "--model".into(),
            context.config.model.clone(),
            "--output-format".into(),
            "json".into(),
            "--yolo".into(),
        ],
        prompt_delivery: PromptDelivery::Argument,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let key = connection.credential.as_deref().unwrap_or("asb-local");
    match connection.interface {
        crate::endpoint::Interface::AnthropicMessages => {
            context
                .command
                .env("QWEN_DEFAULT_AUTH_TYPE", "anthropic")
                .env("ANTHROPIC_API_KEY", key)
                .env(
                    "ANTHROPIC_BASE_URL",
                    connection.base_url.trim_end_matches("/v1"),
                )
                .env("ANTHROPIC_MODEL", context.model)
                .env_remove("OPENAI_API_KEY")
                .env_remove("OPENAI_BASE_URL")
                .env_remove("OPENAI_API_BASE")
                .env_remove("OPENAI_MODEL");
        }
        crate::endpoint::Interface::OpenAiChat => {
            context
                .command
                .env("QWEN_DEFAULT_AUTH_TYPE", "openai")
                .env("OPENAI_API_KEY", key)
                .env("OPENAI_BASE_URL", &connection.base_url)
                .env("OPENAI_MODEL", context.model)
                .env_remove("ANTHROPIC_API_KEY")
                .env_remove("ANTHROPIC_BASE_URL");
        }
        _ => return Err(context.unsupported_endpoint("qwen-code")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn selects_final_cumulative_usage_not_intermediate_messages_or_nested_stats() {
        let output = r#"[
            {"type":"message","usage":{"input_tokens":0,"output_tokens":0}},
            {"type":"message","usage":{"input_tokens":17785,"output_tokens":73}},
            {"type":"result","usage":{"input_tokens":108750,"output_tokens":1037,"cache_read_input_tokens":11520,"total_tokens":109787},"stats":{"models":{"m":{"tokens":{"prompt":108750,"candidates":1037,"total":109787}}}}}
        ]"#;
        let usage = crate::usage::from_sources("openrouter", "qwen-code", output, "", None);
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cached_tokens,
                usage.total_tokens
            ),
            (Some(108750), Some(1037), Some(11520), Some(109787))
        );
        assert_eq!(usage.source.as_deref(), Some("agent-output"));
        let unknown = crate::usage::from_sources(
            "openrouter",
            "qwen-code",
            r#"[{"type":"message","usage":{"input_tokens":0,"output_tokens":0}},{"type":"result","usage":{"input_tokens":0,"output_tokens":0}}]"#,
            "",
            None,
        );
        assert_eq!(unknown.total_tokens, None);
        assert_eq!(unknown.input_tokens, None);
    }
}
