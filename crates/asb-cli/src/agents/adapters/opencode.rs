use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, PreparationContext,
    PromptDelivery, StaticAdapter,
};
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

// OpenCode discovers locally configured MCP tools asynchronously. A short
// startup budget can leave the first provider request tool-less, making a
// recorded request shape impossible to replay deterministically. Keep this
// readiness policy in the owning adapter configuration.
const LOCAL_TOOL_STARTUP_TIMEOUT_MS: u64 = 60_000;

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
.with_estimated_disk_mib(1024)
.with_complete_tool_replay(prepare_tool_replay)
.with_artifact_specs(&[
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "local-tools.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "tool-protocol.json",
        format: super::super::ArtifactFormat::Json,
    },
])
.with_collection(collect)
.with_output_usage(output_usage)
.with_final_response(final_response)
.with_step_count(step_count)
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

fn final_response(output: &str, _: &std::path::Path) -> Option<String> {
    let mut texts = std::collections::HashMap::<String, String>::new();
    let mut final_answer = None;
    for event in output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        let kind = event.get("type").and_then(|v| v.as_str());
        let part = event.get("part");
        match kind {
            Some("text") => {
                if let (Some(id), Some(text)) = (
                    part.and_then(|p| p.get("messageID"))
                        .and_then(|v| v.as_str()),
                    part.and_then(|p| p.get("text")).and_then(|v| v.as_str()),
                ) {
                    if !text.trim().is_empty() {
                        texts.insert(id.to_owned(), text.to_owned());
                    }
                }
                final_answer = None;
            }
            Some("step_finish") => {
                final_answer = match (
                    part.and_then(|p| p.get("messageID"))
                        .and_then(|v| v.as_str()),
                    part.and_then(|p| p.get("reason")).and_then(|v| v.as_str()),
                ) {
                    (Some(id), Some(reason))
                        if !reason.is_empty()
                            && !matches!(reason, "tool-calls" | "unknown" | "content-filter") =>
                    {
                        texts.get(id).cloned()
                    }
                    _ => None,
                };
            }
            Some("step_start" | "tool_use" | "error") => final_answer = None,
            _ => {}
        }
    }
    final_answer
}

fn output_usage(stdout: &str, _stderr: &str, _: bool) -> crate::measurements::TokenUsage {
    let mut sum = crate::measurements::TokenUsage::default();
    let mut seen = false;
    let mut native_stream = false;
    let mut cache_complete = true;
    for event in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        let event_type = event.get("type").and_then(|v| v.as_str());
        native_stream |= matches!(
            event_type,
            Some("tool_use" | "step_start" | "step_finish" | "text" | "reasoning" | "error")
        );
        if event_type != Some("step_finish") {
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
    } else if native_stream {
        // An incomplete native JSON stream has no authoritative completed
        // usage; tool input and task text may themselves contain token data.
        Default::default()
    } else {
        Default::default()
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
    // The tool hook runs before the adapter hook.  Consume its private launch
    // description before mutating the command, then remove the bridge
    // variable so it is never exposed to OpenCode or the benchmark process.
    let tool_command = tool_replay(context)?;
    context
        .command
        .env_remove("ASB_OPENCODE_TOOL_REPLAY_COMMAND");
    context
        .command
        .env(
            "ASB_AGENT_API_KEY",
            connection.credential.as_deref().unwrap_or("asb-local"),
        )
        .env(
            "OPENCODE_CONFIG_CONTENT",
            config(context.model, &connection.base_url, npm, tool_command)?,
        );
    // OpenCode's repository-search tool invokes ripgrep.  Fresh workload
    // materialization can give the same files a different directory-entry
    // order, which otherwise changes only the ordering of an identical tool
    // result and breaks strict replay history matching.  Keep this policy in
    // the owning adapter and use a private runtime file, never a host-wide
    // ripgrep configuration.
    std::fs::create_dir_all(context.runtime_dir).map_err(|error| {
        Error::Run(format!(
            "cannot create deterministic OpenCode runtime directory: {error}"
        ))
    })?;
    let ripgrep_config = context.runtime_dir.join("opencode-ripgrep.conf");
    std::fs::write(&ripgrep_config, "--sort=path\n").map_err(|error| {
        Error::Run(format!(
            "cannot write deterministic OpenCode search configuration: {error}"
        ))
    })?;
    context.command.env("RIPGREP_CONFIG_PATH", ripgrep_config);
    Ok(())
}

fn prepare_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    super::super::tool_replay_mcp::prepare_protocol_artifact(context, tape)?;
    let launch = super::super::tool_replay_mcp::launch(context, tape)?;
    let command = std::iter::once(launch.program)
        .chain(launch.args.into_iter().map(std::path::PathBuf::from))
        .map(|value| {
            value
                .into_os_string()
                .into_string()
                .map_err(|_| Error::Run("ASB local-tool replay argument is not valid UTF-8".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    context.command.env(
        "ASB_OPENCODE_TOOL_REPLAY_COMMAND",
        serde_json::to_string(&command)
            .map_err(|_| Error::Run("cannot encode OpenCode local-tool command".into()))?,
    );
    Ok(())
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let mut artifacts = AgentArtifacts::default();
    if let Some(bytes) = context.frozen.bytes("tool-protocol.json") {
        if let Err(error) = super::super::tool_replay_mcp::verify_protocol_bytes(bytes) {
            artifacts.errors.push(error.to_string());
        } else {
            artifacts
                .sealed_artifacts
                .push(super::super::SealedArtifact {
                    kind: "tool-protocol".into(),
                    bytes: bytes.to_vec(),
                });
        }
    }
    artifacts
}

fn tool_replay(context: &PreparationContext<'_>) -> Result<Option<Vec<String>>, Error> {
    let Some(command) = context
        .command
        .get_envs()
        .find_map(|(name, value)| (name == "ASB_OPENCODE_TOOL_REPLAY_COMMAND").then_some(value))
        .flatten()
    else {
        return Ok(None);
    };
    serde_json::from_slice(command.as_encoded_bytes())
        .map(Some)
        .map_err(|_| Error::Run("OpenCode local-tool command is malformed".into()))
}

fn config(
    model: &str,
    base_url: &str,
    npm: &str,
    tool_command: Option<Vec<String>>,
) -> Result<String, Error> {
    let mut models = serde_json::Map::new();
    models.insert(model.to_owned(), serde_json::json!({"name": model}));
    let mut configuration = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {"asb": {
            "npm": npm, "name": "ASB provider",
            "options": {"baseURL": base_url, "apiKey": "{env:ASB_AGENT_API_KEY}"},
            "models": models
        }}
    });
    if let Some(command) = tool_command {
        configuration["mcp"] = serde_json::json!({
            "asb_local_tools": {
                "type":"local",
                "command":command,
                "enabled":true,
                "timeout":LOCAL_TOOL_STARTUP_TIMEOUT_MS
            }
        });
        // The public OpenCode configuration exposes these as every built-in
        // executable/action tool.  Denying them leaves the sealed ASB MCP
        // service as the sole benchmark-visible local-tool authority.
        configuration["permission"] = serde_json::json!({
            "bash":"deny", "edit":"deny", "read":"deny", "grep":"deny",
            "glob":"deny", "lsp":"deny", "apply_patch":"deny", "skill":"deny",
            "todowrite":"deny", "webfetch":"deny", "websearch":"deny", "question":"deny",
            "asb_local_tools_*":"allow"
        });
    }
    serde_json::to_string(&configuration)
        .map_err(|error| Error::Run(format!("cannot configure OpenCode provider: {error}")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn final_response_requires_matching_terminal_step() {
        let text = r#"{"type":"text","part":{"messageID":"m1","text":"answer"}}"#;
        let finish = r#"{"type":"step_finish","part":{"messageID":"m1","reason":"stop"}}"#;
        let tools = r#"{"type":"step_finish","part":{"messageID":"m1","reason":"tool-calls"}}"#;
        let wrong = r#"{"type":"step_finish","part":{"messageID":"m2","reason":"stop"}}"#;
        let path = std::path::Path::new(".");
        assert_eq!(
            super::final_response(&format!("{text}\n{finish}"), path).as_deref(),
            Some("answer")
        );
        for output in [
            text.to_owned(),
            format!("{text}\n{tools}"),
            format!("{text}\n{wrong}"),
            format!("{text}\n{finish}\n{{\"type\":\"error\"}}"),
        ] {
            assert!(super::final_response(&output, path).is_none(), "{output}");
        }
    }

    #[test]
    fn incomplete_stream_does_not_count_tool_input_as_usage() {
        let output = r#"{"type":"tool_use","part":{"type":"tool","state":{"status":"completed","input":{"tokens":{"input":999,"output":888}}}}}"#;
        assert!(!super::output_usage(output, "", false).any());
    }

    #[test]
    fn task_tokens_are_not_usage_without_completed_step() {
        let output = r#"{"type":"tool_use","part":{"state":{"output":{"tokens":{"input":999,"output":888}}}}}"#;
        let usage = crate::usage::from_sources("openrouter", "opencode", output, "", None);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (None, None, None)
        );
    }
    use super::*;

    #[test]
    fn provider_configuration_is_owned_by_the_opencode_adapter() {
        let config = config(
            "test-model",
            "http://127.0.0.1:3456/v1",
            "@ai-sdk/anthropic",
            None,
        )
        .expect("configuration succeeds");
        assert!(config.contains("test-model"));
        assert!(config.contains("ASB_AGENT_API_KEY"));
    }

    #[test]
    fn tool_configuration_seals_native_tools_behind_the_asb_mcp_service() {
        let config = config(
            "test-model",
            "http://127.0.0.1:3456/v1",
            "@ai-sdk/openai-compatible",
            Some(vec!["asb-tool-replay-mcp".into(), "--tape".into()]),
        )
        .expect("configuration succeeds");
        let config: serde_json::Value = serde_json::from_str(&config).unwrap();
        assert_eq!(
            config["mcp"]["asb_local_tools"]["command"],
            serde_json::json!(["asb-tool-replay-mcp", "--tape"])
        );
        assert_eq!(config["mcp"]["asb_local_tools"]["enabled"], true);
        assert_eq!(
            config["mcp"]["asb_local_tools"]["timeout"],
            LOCAL_TOOL_STARTUP_TIMEOUT_MS
        );
        assert_eq!(config["permission"]["bash"], "deny");
        assert_eq!(config["permission"]["asb_local_tools_*"], "allow");
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
            mcp_servers: &[],
            mcp_tape: None,
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
