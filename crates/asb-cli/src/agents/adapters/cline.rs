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
.with_estimated_disk_mib(1024)
.with_complete_tool_replay(prepare_tool_replay)
.with_artifact_specs(&[super::super::ArtifactSpec {
    namespace: "tool-replay",
    filename: "local-tools.jsonl",
    format: super::super::ArtifactFormat::JsonLines,
}])
.with_replay_credential_vars(&["CLINE_API_KEY"])
.with_state_environment_vars(&[
    "CLINE_DIR",
    "CLINE_DATA_DIR",
    "CLINE_PROVIDER_SETTINGS_PATH",
    "CLINE_GLOBAL_SETTINGS_PATH",
    "CLINE_MCP_SETTINGS_PATH",
])
.with_output_usage(output_usage)
.with_final_response(final_response)
.with_step_count(step_count);

fn final_response(output: &str, _: &std::path::Path) -> Option<String> {
    let mut answer = None;
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let event: serde_json::Value = serde_json::from_str(line).ok()?;
        match event.get("type").and_then(|value| value.as_str()) {
            Some("run_result") => {
                answer = (event.get("finishReason").and_then(|value| value.as_str())
                    == Some("completed"))
                .then(|| event.get("text").and_then(|value| value.as_str()))
                .flatten()
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned);
            }
            Some("run_aborted" | "error") => answer = None,
            _ => {}
        }
    }
    answer
}

fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    stdout
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|event| event.get("type").and_then(|v| v.as_str()) == Some("run_result"))
        .and_then(|event| event.get("iterations").and_then(|v| v.as_u64()))
}

fn output_usage(stdout: &str, _stderr: &str, _: bool) -> crate::measurements::TokenUsage {
    for event in stdout
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if event.get("type").and_then(|v| v.as_str()) == Some("run_result") {
            return event
                .get("aggregateUsage")
                .or_else(|| event.get("usage"))
                .map(super::super::usage_fields)
                .unwrap_or_default()
                .with_derived_total();
        }
    }
    // Intermediate events and task output are not final aggregate usage.
    Default::default()
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
        prompt_delivery: super::PromptDelivery::Embedded,
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
    let sealed_tools = context
        .command
        .get_envs()
        .find_map(|(name, value)| (name == "ASB_CLINE_SEALED_TOOLS").then_some(value))
        .flatten()
        .is_some_and(|value| value == "1");
    context.command.env_remove("ASB_CLINE_SEALED_TOOLS");
    if sealed_tools {
        write_sealed_tool_settings(&directory)?;
    }
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

/// Cline's headless configuration exposes every native coding action under a
/// stable tool identifier.  Disable that complete set for tape-backed runs
/// and publish the ASB stdio MCP endpoint as the only remaining local-action
/// authority.  MCP tools are deliberately not part of `disabledTools`.
fn prepare_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    let launch = super::super::tool_replay_mcp::launch(context, tape)?;
    let directory = context.runtime_dir.join("cline/config/data/settings");
    std::fs::create_dir_all(&directory).map_err(|error| {
        Error::Run(format!(
            "cannot create Cline local-tool configuration directory: {error}"
        ))
    })?;
    let mcp_path = directory.join("cline_mcp_settings.json");
    let mcp = serde_json::json!({
        "mcpServers": {"asb_local_tools": {
            "command": launch.program.to_string_lossy(),
            "args": launch.args.iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            "disabled": false,
            "autoApprove": ["list_directory", "search_text", "run_command", "read_file", "write_file", "create_directory", "remove_file"],
        }}
    });
    std::fs::write(
        &mcp_path,
        serde_json::to_vec(&mcp)
            .map_err(|error| Error::Run(format!("cannot encode Cline local tools: {error}")))?,
    )
    .map_err(|error| Error::Run(format!("cannot write Cline local tools: {error}")))?;
    context.command.env("CLINE_MCP_SETTINGS_PATH", mcp_path);

    context.command.env("ASB_CLINE_SEALED_TOOLS", "1");
    Ok(())
}

fn write_sealed_tool_settings(directory: &std::path::Path) -> Result<(), Error> {
    let global_path = directory.join("data/settings/global-settings.json");
    let global = serde_json::json!({
        "disabledTools": [
            "read_files", "search_codebase", "run_commands", "editor",
            "fetch_web_content", "skills", "ask_question", "spawn_agent", "teams"
        ],
        "tools": {"web_search": {"enabled": false}, "image_generation": {"enabled": false}}
    });
    std::fs::write(
        global_path,
        serde_json::to_vec(&global)
            .map_err(|error| Error::Run(format!("cannot encode sealed Cline tools: {error}")))?,
    )
    .map_err(|error| Error::Run(format!("cannot write sealed Cline tools: {error}")))?;
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
    #[test]
    fn only_completed_run_result_is_final_response() {
        use crate::agents::AgentAdapter;
        let path = std::path::Path::new(".");
        let completed = r#"{"type":"run_result","finishReason":"completed","text":"final answer"}"#;
        assert_eq!(
            super::ADAPTER.final_response(completed, path).as_deref(),
            Some("final answer")
        );
        for event in [
            r#"{"type":"agent_event","event":{"type":"content_end","text":"intermediate"}}"#,
            r#"{"type":"run_result","finishReason":"aborted","text":"unfinished"}"#,
            r#"{"type":"run_result","finishReason":"error","text":"failed"}"#,
            r#"{"type":"run_result","text":"unknown"}"#,
        ] {
            assert!(
                super::ADAPTER.final_response(event, path).is_none(),
                "{event}"
            );
        }
        let aborted = format!("{completed}\n{{\"type\":\"run_aborted\"}}");
        assert!(super::ADAPTER.final_response(&aborted, path).is_none());
    }
    #[test]
    fn tool_event_is_not_usage_when_final_result_has_no_usage() {
        let output = concat!(
            "{\"type\":\"agent_event\",\"event\":{\"type\":\"tool_result\",\"content\":\"{\\\"usage\\\":{\\\"input_tokens\\\":999,\\\"output_tokens\\\":888}}\"}}\n",
            "{\"type\":\"run_result\",\"iterations\":1}\n"
        );
        let usage = crate::usage::from_sources("openrouter", "cline", output, "", None);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (None, None, None)
        );
    }

    #[test]
    fn newer_incomplete_result_does_not_reuse_an_earlier_result() {
        let output = concat!(
            "{\"type\":\"run_result\",\"usage\":{\"inputTokens\":10,\"outputTokens\":3}}\n",
            "{\"type\":\"run_result\",\"iterations\":2}\n",
        );
        let usage = crate::usage::from_sources("openrouter", "cline", output, "", None);
        assert_eq!(usage.total_tokens, None);
    }
    use super::*;
    use crate::agents::AgentAdapter;

    #[test]
    fn tape_backed_preparation_seals_native_tools_behind_the_private_mcp() {
        let root = std::env::temp_dir().join(format!(
            "asb-cline-tool-replay-{}-{}",
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
        let mut command = std::process::Command::new("cline");
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
        let settings = root.join("runtime/cline/config/data/settings");
        let mcp: serde_json::Value = serde_json::from_slice(
            &std::fs::read(settings.join("cline_mcp_settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            mcp["mcpServers"]["asb_local_tools"]["args"][0],
            "__asb-tool-replay"
        );
        let global: serde_json::Value =
            serde_json::from_slice(&std::fs::read(settings.join("global-settings.json")).unwrap())
                .unwrap();
        assert!(
            global["disabledTools"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("run_commands"))
        );
        assert!(
            global["disabledTools"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("editor"))
        );
        assert!(
            command
                .get_envs()
                .any(|(name, value)| { name == "ASB_CLINE_SEALED_TOOLS" && value.is_none() })
        );
        assert!(run_dir.join("tool-replay/local-tools.jsonl").is_file());
        let _ = std::fs::remove_dir_all(root);
    }

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
