use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, PreparationContext,
    PromptDelivery, StaticAdapter,
};
use crate::{Error, install::Recipe};
const TOOL_PROGRESS_FILENAME: &str = "persistent-progress.json";

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
.with_complete_tool_replay(prepare_tool_replay)
.with_collection(collect)
.with_artifact_specs(&[
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "local-tools.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        // Qwen restarts its private MCP child.  Its cursor is the bounded receipt
        // that lets deferred host collection verify the sealed transcript after
        // provider tape settlement.
        namespace: "tool-replay",
        filename: TOOL_PROGRESS_FILENAME,
        format: super::super::ArtifactFormat::Json,
    },
])
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
    fn latest_result(value: &serde_json::Value) -> Option<&serde_json::Value> {
        match value {
            serde_json::Value::Array(events) => events.iter().rev().find_map(latest_result),
            serde_json::Value::Object(event)
                if event.get("type").and_then(|v| v.as_str()) == Some("result") =>
            {
                Some(value)
            }
            _ => None,
        }
    }
    let document = serde_json::from_str::<serde_json::Value>(output).ok();
    let events = if let Some(document) = document {
        vec![document]
    } else {
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?
    };
    let result = events.iter().rev().find_map(latest_result)?;
    if result.get("is_error").and_then(|v| v.as_bool()) != Some(false) {
        return None;
    }
    result
        .get("result")
        .and_then(|v| v.as_str())
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    match super::super::tool_replay_mcp::verify_persistent_per_call_completion_receipt(
        context.frozen.document(TOOL_PROGRESS_FILENAME),
        context.frozen.bytes("local-tools.jsonl"),
    ) {
        Ok(()) => AgentArtifacts::default(),
        Err(error) => AgentArtifacts {
            errors: vec![error.to_string()],
            ..AgentArtifacts::default()
        },
    }
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

/// Qwen Code accepts a per-invocation MCP document.  Its explicit exclusion
/// list removes every native workspace/shell action while retaining the ASB
/// MCP server, whose responses are the sole local-tool observations during a
/// tape-backed attempt.
fn prepare_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    // Qwen may replace the configured stdio MCP child after a tool response.
    // Keep the sealed transcript and replay cursor across those restarts.
    let launch =
        super::super::tool_replay_mcp::per_call_launch(context, tape, TOOL_PROGRESS_FILENAME)?;
    let directory = context.runtime_dir.join("qwen-code");
    std::fs::create_dir_all(&directory)
        .map_err(|error| Error::Run(format!("cannot create Qwen local-tool directory: {error}")))?;
    let path = directory.join("asb-local-tools.json");
    let document = serde_json::json!({
        "mcpServers": {"asb_local_tools": {
            "command": launch.program.to_string_lossy(),
            "args": launch.args.iter().map(|argument| argument.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            "trust": true,
            "includeTools": ["list_directory", "search_text", "run_command", "read_file", "write_file", "create_directory", "remove_file"],
        }}
    });
    std::fs::write(
        &path,
        serde_json::to_vec(&document)
            .map_err(|error| Error::Run(format!("cannot encode Qwen local tools: {error}")))?,
    )
    .map_err(|error| Error::Run(format!("cannot write Qwen local tools: {error}")))?;
    context.command.args([
        "--mcp-config",
        path.to_str().ok_or_else(|| Error::Run("Qwen local-tool path is not valid UTF-8".into()))?,
        "--allowed-mcp-server-names",
        "asb_local_tools",
        "--exclude-tools",
        "read_file,write_file,list_directory,run_shell_command,search_files,search_codebase,replace,apply_patch,web_fetch,web_search,computer_use,agent",
    ]);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_successful_qwen_result_is_final() {
        let path = std::path::Path::new(".");
        let success = r#"[{"type":"result","is_error":false,"result":"done"}]"#;
        assert_eq!(
            super::final_response(success, path).as_deref(),
            Some("done")
        );
        let error = r#"[{"type":"result","is_error":true,"result":"failure"}]"#;
        assert!(super::final_response(error, path).is_none());
        for event in [
            r#"[{"type":"assistant","content":"intermediate"}]"#,
            r#"[{"type":"result","result":"unknown"}]"#,
            r#"[{"type":"result","is_error":false,"result":"done"},{"type":"result","is_error":true,"result":"failure"}]"#,
            r#"[{"type":"result","is_error":false,"result":"done"},{"type":"result","is_error":false}]"#,
        ] {
            assert!(super::final_response(event, path).is_none(), "{event}");
        }
        let stream = "{\"type\":\"assistant\",\"content\":\"thinking\"}\n{\"type\":\"result\",\"is_error\":false,\"result\":\"done\"}";
        assert_eq!(super::final_response(stream, path).as_deref(), Some("done"));
        assert!(super::final_response(&format!("{stream}\n{{"), path).is_none());
    }
    use super::*;
    use crate::agents::AgentAdapter;

    fn frozen_collection_artifacts(
        progress: serde_json::Value,
        transcript: Vec<u8>,
    ) -> crate::agents::FrozenArtifacts {
        let envelope = serde_json::json!({
            "schema_version": 1,
            "documents": {"persistent-progress.json": progress},
            "bytes": {"local-tools.jsonl": transcript},
        });
        crate::agents::FrozenArtifacts::from_deferred_collection_bytes(
            &serde_json::to_vec(&envelope).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn deferred_collection_uses_the_exported_persistent_mcp_receipt() {
        let root = std::env::temp_dir().join(format!(
            "asb-qwen-deferred-receipt-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let frozen = frozen_collection_artifacts(
            serde_json::json!({"schema_version": 1, "cursor": 0, "events": 0}),
            Vec::new(),
        );
        let artifacts = collect(&CollectionContext {
            frozen: &frozen,
            provider_capture: None,
            run_dir: &root,
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
        });
        assert!(artifacts.errors.is_empty(), "{:?}", artifacts.errors);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn deferred_collection_rejects_an_unconsumed_persistent_mcp_receipt() {
        let root = std::env::temp_dir().join(format!(
            "asb-qwen-deferred-receipt-reject-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let frozen = frozen_collection_artifacts(
            serde_json::json!({"schema_version": 1, "cursor": 0, "events": 1}),
            Vec::new(),
        );
        let artifacts = collect(&CollectionContext {
            frozen: &frozen,
            provider_capture: None,
            run_dir: &root,
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
        });
        assert_eq!(
            artifacts.errors,
            ["local-tool replay left recorded events unconsumed"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tape_backed_preparation_uses_only_the_private_mcp_transport() {
        let root = std::env::temp_dir().join(format!(
            "asb-qwen-tool-replay-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        let run_dir = root.join("run");
        std::fs::create_dir_all(&workspace).unwrap();
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("asb-replay".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let mut command = std::process::Command::new("qwen");
        ADAPTER
            .prepare(&mut PreparationContext {
                command: &mut command,
                model: "test-model",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &root.join("runtime"),
                run_dir: &run_dir,
                mcp_servers: &[],
                mcp_tape: None,
                tape: Some(super::super::TapeContext {
                    mode: crate::tape::Mode::Record,
                    cassette: &root.join("recording.asb.zip"),
                }),
            })
            .unwrap();
        let arguments = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(arguments.contains(&"--mcp-config".into()));
        assert!(
            arguments
                .iter()
                .any(|value| value.contains("run_shell_command"))
        );
        let config: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("runtime/qwen-code/asb-local-tools.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            config["mcpServers"]["asb_local_tools"]["args"][0],
            "__asb-tool-replay"
        );
        assert!(run_dir.join("tool-replay/local-tools.jsonl").is_file());
        assert!(
            run_dir
                .join("tool-replay/persistent-progress.json")
                .is_file()
        );
        let _ = std::fs::remove_dir_all(root);
    }

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
