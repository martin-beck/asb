use super::{CommandContext, CommandSpec, PreparationContext, StaticAdapter};
use crate::{Error, install::Recipe};
use std::path::Path;
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "pi",
    "pi",
    Recipe::Npm {
        package: "@mariozechner/pi-coding-agent@0.73.1",
        executable: "pi",
        manifest: None,
    },
    &[crate::endpoint::RESPONSES, crate::endpoint::CHAT],
    command,
    prepare_adapter,
)
.with_output_usage(output_usage)
.with_state_environment_vars(&["PI_CODING_AGENT_DIR"])
.with_execution_failure(execution_failure)
.with_step_count(step_count);

fn execution_failure(stdout: &str, _: &str) -> Option<String> {
    let mut failed = false;
    for event in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        let message = if event["type"] == "turn_end" {
            event.get("message")
        } else if event["type"] == "agent_end" {
            event
                .get("messages")
                .and_then(|messages| messages.as_array())
                .and_then(|messages| {
                    messages
                        .iter()
                        .rev()
                        .find(|message| message["role"] == "assistant")
                })
        } else {
            None
        };
        if let Some(reason) = message
            .and_then(|message| message.get("stopReason"))
            .and_then(|reason| reason.as_str())
        {
            failed = matches!(reason, "error" | "aborted");
        }
    }
    failed.then(|| "agent reported a terminal API request failure or cancellation; inspect the private agent log".into())
}

#[cfg(test)]
mod execution_tests {
    use super::execution_failure;

    #[test]
    fn terminal_api_errors_are_failures_without_echoing_provider_bodies() {
        let output = serde_json::json!({"type":"agent_end","messages":[{"role":"assistant",
            "stopReason":"error","errorMessage":"400 private-provider-body"}]})
        .to_string();
        let failure = execution_failure(&output, "").unwrap();
        assert!(failure.contains("API request"));
        assert!(!failure.contains("private-provider-body"));
    }

    #[test]
    fn recovered_turn_is_not_failed_by_an_earlier_error() {
        let output = [
            serde_json::json!({"type":"turn_end","message":{"stopReason":"error"}}),
            serde_json::json!({"type":"turn_end","message":{"stopReason":"stop"}}),
        ]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        assert!(execution_failure(&output, "").is_none());
    }
}

fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    let (mut active, mut turn_open, mut completed) = (false, false, false);
    let (mut retry_pending, mut compaction_pending) = (false, false);
    let mut continuation_pending = false;
    let mut turns = 0_u64;
    let mut cycle_turns = 0_u64;
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let event: serde_json::Value = serde_json::from_str(line).ok()?;
        match event.get("type")?.as_str()? {
            "agent_start" if !active => {
                active = true;
                completed = false;
                continuation_pending = false;
                cycle_turns = 0;
            }
            "turn_start" if active && !turn_open => turn_open = true,
            "turn_end"
                if active
                    && turn_open
                    && event.pointer("/message/role").and_then(|v| v.as_str())
                        == Some("assistant") =>
            {
                // One terminal event per attempted main-agent model turn.
                // Error/aborted turns count too; agent_end repeats messages.
                turns = turns.checked_add(1)?;
                cycle_turns = cycle_turns.checked_add(1)?;
                turn_open = false;
            }
            "agent_end" if active && !turn_open && cycle_turns > 0 => {
                active = false;
                completed = true;
            }
            "agent_start" | "turn_start" | "turn_end" | "agent_end" => return None,
            "auto_retry_start" => retry_pending = true,
            "auto_retry_end" => retry_pending = false,
            // Retry starts may repeat across failed attempts, and terminal
            // retry/compaction errors can end without a start in Pi itself.
            "compaction_start" => compaction_pending = true,
            "compaction_end" => {
                compaction_pending = false;
                continuation_pending = event.get("willRetry")?.as_bool()?;
            }
            _ => {}
        }
    }
    (completed
        && !active
        && !turn_open
        && !retry_pending
        && !compaction_pending
        && !continuation_pending
        && turns > 0)
        .then_some(turns)
}

fn output_usage(stdout: &str, stderr: &str, _: bool) -> crate::measurements::TokenUsage {
    let mut sum = crate::measurements::TokenUsage::default();
    let mut seen = false;
    let mut cache_complete = true;
    for event in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if event.get("type").and_then(|v| v.as_str()) != Some("turn_end") {
            continue;
        }
        let Some(value) = event
            .get("usage")
            .or_else(|| event.pointer("/message/usage"))
        else {
            return Default::default();
        };
        let number = |key: &str| value.get(key).and_then(|v| v.as_u64());
        let (Some(input), Some(output)) = (number("input"), number("output")) else {
            return Default::default();
        };
        let read = number("cacheRead");
        cache_complete &= read.is_some();
        sum = sum.add(crate::measurements::TokenUsage {
            input_tokens: Some(
                input
                    .saturating_add(read.unwrap_or(0))
                    .saturating_add(number("cacheWrite").unwrap_or(0)),
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
    CommandSpec {
        program: "pi".into(),
        args: vec![
            "--provider".into(),
            "asb".into(),
            "--model".into(),
            context.config.model.clone(),
            "--mode".into(),
            "json".into(),
            "--print".into(),
        ],
        append_prompt: true,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let directory = context.runtime_dir.join("pi/config");
    let api = match connection.interface {
        crate::endpoint::Interface::OpenAiResponses => "openai-responses",
        crate::endpoint::Interface::OpenAiChat => "openai-completions",
        _ => return Err(context.unsupported_endpoint("pi")),
    };
    context.command.env(
        "ASB_AGENT_API_KEY",
        connection.credential.as_deref().unwrap_or("asb-local"),
    );
    configure_provider(
        context.command,
        &directory,
        "asb",
        context.model,
        &connection.base_url,
        api,
        "ASB_AGENT_API_KEY",
    )
}

fn configure_provider(
    command: &mut std::process::Command,
    directory: &Path,
    provider: &str,
    model: &str,
    base_url: &str,
    api: &str,
    api_key: &str,
) -> Result<(), Error> {
    std::fs::create_dir_all(directory).map_err(|error| {
        Error::Run(format!(
            "cannot create Pi provider configuration directory: {error}"
        ))
    })?;
    let configuration = serde_json::json!({
        "providers": { provider: {
            "baseUrl": base_url, "api": api, "apiKey": api_key,
            "models": [{
                "id": model, "name": model, "reasoning": true, "input": ["text"],
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                "contextWindow": 128000, "maxTokens": 16384
            }]
        }}
    });
    let path = directory.join("models.json");
    let content = serde_json::to_vec_pretty(&configuration)
        .map_err(|error| Error::Run(format!("cannot render Pi provider configuration: {error}")))?;
    std::fs::write(&path, content)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", path.display())))?;
    command.env("PI_CODING_AGENT_DIR", directory);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_only_balanced_terminal_turns_in_complete_agent_cycles() {
        let cycle = r#"{"type":"agent_start"}
{"type":"turn_start"}
{"type":"message_end","message":{"role":"assistant"}}
{"type":"tool_execution_end"}
{"type":"turn_end","message":{"role":"assistant","stopReason":"error"}}
{"type":"agent_end","messages":[{"role":"assistant"},{"role":"assistant"}]}"#;
        assert_eq!(step_count(cycle, ""), Some(1));
        let retry = format!(
            "{cycle}\n{{\"type\":\"auto_retry_start\"}}\n{cycle}\n{{\"type\":\"auto_retry_end\"}}"
        );
        assert_eq!(step_count(&retry, ""), Some(2));
        assert_eq!(
            step_count(&format!("{cycle}\n{{\"type\":\"auto_retry_start\"}}"), ""),
            None
        );
        assert_eq!(
            step_count(&format!("{cycle}\n{{\"type\":\"compaction_start\"}}"), ""),
            None
        );
        assert_eq!(
            step_count(&format!("{cycle}\n{{\"type\":\"turn_end\"}}"), ""),
            None
        );
        assert_eq!(step_count(&format!("{cycle}\nbroken"), ""), None);
        assert_eq!(step_count(cycle.rsplit_once('\n').unwrap().0, ""), None);
        assert_eq!(
            step_count(r#"{"type":"agent_end","messages":[]}"#, ""),
            None
        );
        assert_eq!(step_count("", ""), None);
        assert_eq!(
            step_count(
                &format!("{cycle}\n{{\"type\":\"agent_start\"}}\n{{\"type\":\"agent_end\"}}"),
                ""
            ),
            None
        );
        let compaction = format!(
            "{cycle}\n{{\"type\":\"compaction_start\"}}\n{{\"type\":\"compaction_end\",\"willRetry\":true}}"
        );
        assert_eq!(step_count(&compaction, ""), None);
        assert_eq!(step_count(&format!("{compaction}\n{cycle}"), ""), Some(2));
        assert_eq!(
            step_count(
                &format!("{cycle}\n{{\"type\":\"compaction_end\",\"willRetry\":false}}"),
                ""
            ),
            Some(1)
        );
        assert_eq!(
            step_count(
                &format!(
                    "{cycle}\n{{\"type\":\"auto_retry_end\",\"success\":false,\"attempt\":0}}"
                ),
                ""
            ),
            Some(1)
        );
        let repeated_retry = format!(
            "{cycle}\n{{\"type\":\"auto_retry_start\",\"attempt\":1}}\n{cycle}\n{{\"type\":\"auto_retry_start\",\"attempt\":2}}\n{cycle}\n{{\"type\":\"auto_retry_end\",\"success\":false,\"attempt\":2}}"
        );
        assert_eq!(step_count(&repeated_retry, ""), Some(3));
    }

    #[test]
    fn provider_configuration_is_owned_by_the_pi_adapter() {
        let directory = std::env::temp_dir().join(format!("asb-pi-config-{}", std::process::id()));
        let mut command = std::process::Command::new("pi");
        configure_provider(
            &mut command,
            &directory,
            "cli2key",
            "test-model",
            "http://127.0.0.1:3456/v1",
            "openai-responses",
            "CLI2KEY_API_KEY",
        )
        .expect("configuration succeeds");
        let text =
            std::fs::read_to_string(directory.join("models.json")).expect("configuration exists");
        assert!(text.contains("test-model"));
        let _ = std::fs::remove_dir_all(directory);
    }
}
