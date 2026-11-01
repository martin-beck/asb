use super::{CommandContext, CommandSpec, PreparationContext, StaticAdapter};
use crate::Error;
use crate::install::Recipe;
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "claude-code",
    "claude",
    Recipe::Npm {
        package: "@anthropic-ai/claude-code@2.1.301",
        executable: "claude",
        manifest: None,
    },
    &[crate::endpoint::MESSAGES_BOTH],
    command,
    prepare_adapter,
)
.with_estimated_disk_mib(1024)
.with_complete_tool_replay(prepare_tool_replay)
.with_artifact_specs(&[super::super::ArtifactSpec {
    namespace: "tool-replay",
    filename: "local-tools.jsonl",
    format: super::super::ArtifactFormat::JsonLines,
}])
.with_mcp_servers()
.with_mcp_relay()
.with_collection(super::super::mcp_relay::collect_artifacts)
.with_replay_credential_vars(&["ANTHROPIC_AUTH_TOKEN"])
.with_state_environment_vars(&["CLAUDE_CONFIG_DIR"])
.with_unprivileged_execution()
.with_output_usage(output_usage)
.with_final_response(final_response)
.with_final_response_failure(super::terminal_refusal)
.with_execution_failure(execution_failure)
.with_step_count(step_count);

fn final_response(output: &str, _: &std::path::Path) -> Option<String> {
    let event: serde_json::Value = serde_json::from_str(output.trim()).ok()?;
    if event.get("type").and_then(|value| value.as_str()) != Some("result") {
        return None;
    }
    super::super::final_text_from_event(&event)
}

fn execution_failure(stdout: &str, _: &str) -> Option<String> {
    let event: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    if event.get("type").and_then(|value| value.as_str()) != Some("result") {
        return None;
    }
    let failed = event.get("is_error").and_then(|value| value.as_bool()) == Some(true)
        || event
            .get("subtype")
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.starts_with("error_"));
    failed.then(|| "agent reported a terminal error; inspect the benchmark log".to_owned())
}

fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    // Claude's final num_turns counts model round trips inside this CLI run.
    super::super::final_result_counter(stdout, "num_turns")
}

fn output_usage(
    stdout: &str,
    _stderr: &str,
    input_includes_cache: bool,
) -> crate::measurements::TokenUsage {
    if let Ok(event) = serde_json::from_str::<serde_json::Value>(stdout.trim())
        && event.get("type").and_then(|v| v.as_str()) == Some("result")
        && let Some(value) = event.get("usage")
    {
        let mut usage = super::super::usage_fields(value);
        if !input_includes_cache {
            usage.input_tokens = usage.input_tokens.map(|n| {
                n.saturating_add(usage.cached_tokens.unwrap_or(0))
                    .saturating_add(
                        value
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0),
                    )
            });
        }
        return usage.with_derived_total();
    }
    // Result text and diagnostics can contain arbitrary task content. Only
    // the top-level typed result usage is an agent-owned usage record.
    Default::default()
}
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "claude".into(),
        args: vec![
            "-p".into(),
            "--bare".into(),
            "--no-session-persistence".into(),
            "--output-format".into(),
            "json".into(),
            "--model".into(),
            context.config.model.clone(),
            "--dangerously-skip-permissions".into(),
            context.prompt.into(),
        ],
        prompt_delivery: super::PromptDelivery::Embedded,
    }
}

fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    // Keep Claude Code's user-level settings (which may pin a local provider
    // endpoint) out of a project-local benchmark.  The adapter owns this
    // isolated config root so tape and live runs use the endpoint prepared by
    // the runner rather than ambient host configuration.
    let config_root = context.runtime_dir.join("claude-code/config");
    std::fs::create_dir_all(&config_root).map_err(|error| {
        Error::Run(format!(
            "cannot create Claude Code project config directory {}: {error}",
            config_root.display()
        ))
    })?;
    configure_mcp_servers(context, &config_root)?;
    context.command.env("CLAUDE_CONFIG_DIR", &config_root);
    let connection = context.connection;
    let key = connection.credential.as_deref().unwrap_or("asb-local");
    context
        .command
        .env(
            "ANTHROPIC_BASE_URL",
            connection.base_url.trim_end_matches("/v1"),
        )
        .env_remove("CLAUDE_CODE_USE_BEDROCK")
        .env_remove("CLAUDE_CODE_USE_VERTEX");
    match connection.authentication {
        crate::endpoint::Authentication::Bearer => {
            context
                .command
                .env("ANTHROPIC_API_KEY", "")
                .env("ANTHROPIC_AUTH_TOKEN", key);
        }
        crate::endpoint::Authentication::ApiKey => {
            context
                .command
                .env("ANTHROPIC_API_KEY", key)
                .env_remove("ANTHROPIC_AUTH_TOKEN");
        }
        crate::endpoint::Authentication::None => {
            context
                .command
                .env("ANTHROPIC_API_KEY", "")
                .env_remove("ANTHROPIC_AUTH_TOKEN");
        }
    }
    Ok(())
}

/// Render Claude Code's JSON configuration from the adapter-neutral declared
/// server launches.  The tool-replay hook writes its private fragment first;
/// this single config file composes it with ordinary declared MCP servers so
/// Claude never receives competing `--mcp-config` inputs.
fn configure_mcp_servers(
    context: &mut PreparationContext<'_>,
    config_root: &std::path::Path,
) -> Result<(), Error> {
    let launches = super::super::mcp_relay::server_launches(
        context.run_dir,
        context.mcp_servers,
        context.mcp_tape,
    )?;
    if launches.is_empty() && context.tape.is_none() {
        return Ok(());
    }
    let mut servers = serde_json::Map::new();
    for launch in launches {
        servers.insert(
            launch.id,
            serde_json::json!({
                "command": launch.program.to_string_lossy(),
                "args": launch.args.into_iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            }),
        );
    }
    if context.tape.is_some() {
        let fragment = context.runtime_dir.join("claude-code/asb-local-tools.json");
        let bytes = std::fs::read(&fragment).map_err(|error| {
            Error::Run(format!(
                "cannot read Claude Code sealed local-tool configuration: {error}"
            ))
        })?;
        let configuration: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
            Error::Run(format!(
                "cannot decode Claude Code sealed local-tool configuration: {error}"
            ))
        })?;
        let local = configuration
            .get("mcpServers")
            .and_then(|value| value.get("asb_local_tools"))
            .cloned()
            .ok_or_else(|| {
                Error::Run("Claude Code sealed local-tool configuration is invalid".into())
            })?;
        servers.insert("asb_local_tools".into(), local);
    }
    let path = config_root.join("asb-mcp.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({"mcpServers": servers})).map_err(|error| {
            Error::Run(format!(
                "cannot encode Claude Code MCP configuration: {error}"
            ))
        })?,
    )
    .map_err(|error| {
        Error::Run(format!(
            "cannot write Claude Code MCP configuration: {error}"
        ))
    })?;
    context.command.args([
        "--mcp-config",
        path.to_str().ok_or_else(|| {
            Error::Run("Claude Code MCP configuration path is not valid UTF-8".into())
        })?,
    ]);
    Ok(())
}

/// Claude Code only exposes MCP tools in print mode when they are explicitly
/// allow-listed.  Couple that allow-list with a deny-list for every native
/// local-action tool, so a taped execution cannot fall back to a shell or
/// workspace mutation outside the ASB transcript.
fn prepare_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    let launch = super::super::tool_replay_mcp::launch(context, tape)?;
    let configuration = serde_json::json!({
        "mcpServers": {"asb_local_tools": {
            "command": launch.program.to_string_lossy(),
            "args": launch.args.iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        }}
    });
    let directory = context.runtime_dir.join("claude-code");
    std::fs::create_dir_all(&directory).map_err(|error| {
        Error::Run(format!(
            "cannot create Claude Code local-tool directory: {error}"
        ))
    })?;
    let path = directory.join("asb-local-tools.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&configuration).map_err(|error| {
            Error::Run(format!("cannot encode Claude Code local tools: {error}"))
        })?,
    )
    .map_err(|error| Error::Run(format!("cannot write Claude Code local tools: {error}")))?;
    // Claude Code does not accept glob patterns in `--allowedTools`.  Its
    // documented `mcp__<server>` selector admits all tools from that server.
    // Keep native tools denied, but admit each sealed MCP server as a unit.
    let mut allowed = vec!["mcp__asb_local_tools".to_owned()];
    allowed.extend(
        context
            .mcp_servers
            .iter()
            .map(|declaration| format!("mcp__{}", declaration.id)),
    );
    context.command.args([
        "--allowedTools",
        &allowed.join(","),
        "--disallowedTools",
        "Bash,Edit,Write,Read,Glob,Grep,WebFetch,WebSearch,Task,Skill,NotebookEdit",
        "--append-system-prompt",
        "For this benchmark, inspect and modify the workspace through the configured MCP tools. Do not answer from assumptions; use the MCP tools to examine the task and apply the solution.",
    ]);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn result_answer_is_not_usage_when_result_has_no_usage() {
        let output = serde_json::json!({"type":"result", "result":"{\"usage\":{\"input_tokens\":999,\"output_tokens\":888}}"}).to_string();
        let usage = crate::usage::from_sources("anthropic", "claude-code", &output, "", None);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (None, None, None)
        );
    }
    use super::*;
    use crate::agents::AgentAdapter;

    #[test]
    fn adapter_reports_final_native_turns_including_error_results() {
        assert_eq!(
            ADAPTER.step_count(
                r#"{"type":"result","subtype":"error_max_turns","num_turns":3}"#,
                ""
            ),
            Some(3)
        );
        assert_eq!(
            ADAPTER.step_count(r#"{"type":"assistant","num_turns":3}"#, ""),
            None
        );
        assert_eq!(
            ADAPTER.step_count(r#"[{"type":"result","num_turns":3},{"type":"result"}]"#, ""),
            None
        );
    }

    #[test]
    fn only_result_documents_supply_claude_terminal_answers() {
        let root = std::path::Path::new(".");
        assert!(
            ADAPTER
                .final_response(
                    r#"{"type":"assistant","role":"assistant","content":"I can't run tests"}"#,
                    root
                )
                .is_none()
        );
        assert_eq!(
            ADAPTER
                .final_response(
                    r#"{"type":"result","is_error":false,"result":"I can't run tests"}"#,
                    root
                )
                .as_deref(),
            Some("I can't run tests")
        );
    }

    #[test]
    fn terminal_error_is_read_only_from_claude_result_metadata() {
        for event in [
            r#"{"type":"result","is_error":true,"result":"private details"}"#,
            r#"{"type":"result","subtype":"error_max_turns"}"#,
        ] {
            let error = ADAPTER
                .execution_failure(event, "")
                .expect("typed terminal error");
            assert!(error.contains("terminal error"));
            assert!(!error.contains("private details"));
        }
        for event in [
            r#"{"type":"result","is_error":false,"result":"litellm.NotFoundError"}"#,
            r#"{"type":"tool_result","is_error":true}"#,
            r#"{"type":"result","result":"error_max_turns"}"#,
        ] {
            assert!(ADAPTER.execution_failure(event, "").is_none());
        }
    }

    #[test]
    fn unattended_execution_uses_the_owned_non_root_identity() {
        assert_eq!(
            ADAPTER.execution_identity(),
            crate::agents::ExecutionIdentity::Unprivileged
        );
        let config: crate::benchmark::BenchmarkConfig = toml::from_str(
            "agent='claude-code'\nworkload='fixture'\nprovider='fixture'\nmodel='fixture'\nworkspace='/work'\n",
        )
        .unwrap();
        let command = ADAPTER.command(&CommandContext {
            executable: "claude",
            config: &config,
            endpoint: None,
            interface: None,
            prompt: "fixture prompt",
            max_steps: None,
            native_tool_mode: None,
            native_tool: None,
        });
        assert!(
            command
                .args
                .iter()
                .any(|argument| argument == "--dangerously-skip-permissions")
        );
    }

    #[test]
    fn tool_replay_is_declared_complete_only_with_a_sealed_mcp_transport() {
        assert!(ADAPTER.supports_tool_replay());
        assert!(ADAPTER.supports_mcp_servers());
        assert!(ADAPTER.supports_mcp_relay());
    }

    #[test]
    fn tape_backed_preparation_allows_only_the_private_mcp_tools() {
        let root = std::env::temp_dir().join(format!(
            "asb-claude-tool-replay-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        let run_dir = root.join("run");
        std::fs::create_dir_all(&workspace).unwrap();
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::AnthropicMessages,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("asb-replay".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let mut command = std::process::Command::new("claude");
        let tape = super::super::TapeContext {
            mode: crate::tape::Mode::Record,
            cassette: &root.join("recording.asb.zip"),
        };
        let declarations = [crate::benchmark::McpServerConfig {
            id: "fixture".into(),
            program: "fixture-mcp".into(),
            args: Vec::new(),
        }];
        prepare_tool_replay(
            &mut PreparationContext {
                command: &mut command,
                model: "test-model",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &root.join("runtime"),
                run_dir: &run_dir,
                mcp_servers: &declarations,
                mcp_tape: None,
                tape: None,
            },
            tape,
        )
        .unwrap();
        prepare_adapter(&mut PreparationContext {
            command: &mut command,
            model: "test-model",
            connection: &connection,
            workspace: &workspace,
            runtime_dir: &root.join("runtime"),
            run_dir: &run_dir,
            mcp_servers: &declarations,
            mcp_tape: None,
            tape: Some(tape),
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
                .any(|argument| argument.contains("mcp__asb_local_tools")
                    && argument.contains("mcp__fixture")
                    && !argument.contains("mcp__asb_local_tools__*")
                    && !argument.contains("mcp__fixture__*"))
        );
        assert!(
            arguments
                .iter()
                .any(|argument| argument == "--append-system-prompt")
        );
        assert!(
            arguments
                .iter()
                .any(|argument| argument.contains("configured MCP tools"))
        );
        assert!(
            arguments
                .iter()
                .any(|value| value.contains("Bash,Edit,Write"))
        );
        let configuration: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("runtime/claude-code/config/asb-mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            configuration["mcpServers"]["asb_local_tools"]["args"][0],
            "__asb-tool-replay"
        );
        assert!(run_dir.join("tool-replay/local-tools.jsonl").is_file());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn declared_mcp_servers_use_one_owned_configuration() {
        let root = std::env::temp_dir().join(format!(
            "asb-claude-mcp-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::AnthropicMessages,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("asb-test".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let declarations = [crate::benchmark::McpServerConfig {
            id: "fixture".into(),
            program: "fixture-mcp".into(),
            args: vec!["--workspace".into(), "fixture".into()],
        }];
        let mut command = std::process::Command::new("claude");
        prepare_adapter(&mut PreparationContext {
            command: &mut command,
            model: "test-model",
            connection: &connection,
            workspace: &workspace,
            runtime_dir: &root.join("runtime"),
            run_dir: &root.join("run"),
            mcp_servers: &declarations,
            mcp_tape: None,
            tape: None,
        })
        .unwrap();
        let arguments = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments
                .iter()
                .filter(|argument| *argument == "--mcp-config")
                .count(),
            1
        );
        let configuration: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("runtime/claude-code/config/asb-mcp.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            configuration["mcpServers"]["fixture"]["command"],
            "fixture-mcp"
        );
        assert_eq!(
            configuration["mcpServers"]["fixture"]["args"],
            serde_json::json!(["--workspace", "fixture"])
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn declared_mcp_recording_substitutes_the_bounded_relay() {
        let root = std::env::temp_dir().join(format!(
            "asb-claude-mcp-record-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::AnthropicMessages,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("asb-test".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let declarations = [crate::benchmark::McpServerConfig {
            id: "fixture".into(),
            program: "must-not-start-directly".into(),
            args: vec!["--fixture".into()],
        }];
        let mut command = std::process::Command::new("claude");
        prepare_adapter(&mut PreparationContext {
            command: &mut command,
            model: "test-model",
            connection: &connection,
            workspace: &workspace,
            runtime_dir: &root.join("runtime"),
            run_dir: &root.join("run"),
            mcp_servers: &declarations,
            mcp_tape: Some(super::super::McpTapeContext {
                mode: crate::tape::Mode::Record,
                cassette: None,
            }),
            tape: None,
        })
        .unwrap();
        let configuration: serde_json::Value = serde_json::from_slice(
            &std::fs::read(root.join("runtime/claude-code/config/asb-mcp.json")).unwrap(),
        )
        .unwrap();
        let server = &configuration["mcpServers"]["fixture"];
        assert_ne!(server["command"], "must-not-start-directly");
        assert_eq!(server["args"][0], "__asb-mcp-relay");
        assert_eq!(server["args"][1], "--record-dir");
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn uses_negotiated_authentication_without_conflicting_credentials() {
        let root = std::env::temp_dir().join(format!("asb-claude-auth-{}", std::process::id()));
        for auth in [
            crate::endpoint::Authentication::Bearer,
            crate::endpoint::Authentication::ApiKey,
        ] {
            let connection = crate::endpoint::Connection {
                interface: crate::endpoint::Interface::AnthropicMessages,
                base_url: "http://127.0.0.1:4242/api/v1".into(),
                credential: Some("fixture-key".into()),
                authentication: auth,
            };
            let mut command = std::process::Command::new("unused");
            prepare_adapter(&mut PreparationContext {
                command: &mut command,
                model: "fixture",
                connection: &connection,
                workspace: &root,
                runtime_dir: &root,
                run_dir: &root,
                mcp_servers: &[],
                mcp_tape: None,
                tape: None,
            })
            .unwrap();
            let env = command
                .get_envs()
                .filter_map(|(key, value)| {
                    value.map(|value| {
                        (
                            key.to_string_lossy().into_owned(),
                            value.to_string_lossy().into_owned(),
                        )
                    })
                })
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:4242/api");
            match auth {
                crate::endpoint::Authentication::Bearer => {
                    assert_eq!(env["ANTHROPIC_API_KEY"], "");
                    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "fixture-key");
                }
                crate::endpoint::Authentication::ApiKey => {
                    assert_eq!(env["ANTHROPIC_API_KEY"], "fixture-key");
                    assert!(!env.contains_key("ANTHROPIC_AUTH_TOKEN"));
                }
                _ => unreachable!(),
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
