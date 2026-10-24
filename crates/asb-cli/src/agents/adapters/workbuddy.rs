use super::{CommandContext, CommandSpec, PreparationContext, StaticAdapter};
use crate::{Error, install::Recipe};
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "workbuddy",
    "codebuddy",
    Recipe::Npm {
        package: "@tencent-ai/codebuddy-code@2.167.0",
        executable: "codebuddy",
        manifest: None,
    },
    &[crate::endpoint::RESPONSES],
    command,
    prepare_adapter,
)
.with_tool_replay(prepare_tool_replay)
.with_artifact_specs(&[super::super::ArtifactSpec {
    namespace: "tool-replay",
    filename: "local-tools.jsonl",
    format: super::super::ArtifactFormat::JsonLines,
}])
.with_replay_credential_vars(&["CODEBUDDY_API_KEY", "CODEBUDDY_AUTH_TOKEN"])
.with_state_environment_vars(&["CODEBUDDY_CONFIG_DIR"])
.with_output_usage(output_usage)
.with_step_count(step_count);

fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    // Use CodeBuddy's own active-agent history counter from its final result.
    super::super::final_result_counter(stdout, "num_turns")
}

fn output_usage(stdout: &str, stderr: &str, _: bool) -> crate::measurements::TokenUsage {
    fn result(value: &serde_json::Value) -> Option<crate::measurements::TokenUsage> {
        match value {
            serde_json::Value::Array(events) => events.iter().rev().find_map(result),
            serde_json::Value::Object(event)
                if event.get("type").and_then(|v| v.as_str()) == Some("result") =>
            {
                event.get("usage").map(super::super::usage_fields)
            }
            _ => None,
        }
    }
    serde_json::from_str::<serde_json::Value>(stdout)
        .ok()
        .and_then(|v| result(&v))
        .unwrap_or_else(|| crate::usage::usage_from_text(&format!("{stdout}\n{stderr}")))
        .with_derived_total()
}
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "codebuddy".into(),
        args: vec![
            "--print".into(),
            "--output-format".into(),
            "json".into(),
            "--model".into(),
            context.config.model.clone(),
            // Keep record and replay histories aligned. CodeBuddy's automatic
            // compaction point otherwise depends on local execution timing.
            "--autocompact".into(),
            "1m".into(),
            "-y".into(),
        ],
        append_prompt: true,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let key = connection.credential.as_deref().unwrap_or("asb-local");
    let base_url = &connection.base_url;
    let directory = context.runtime_dir.join("workbuddy/config");
    configure(&directory, context.model, base_url)?;
    context
        .command
        .env("CODEBUDDY_API_KEY", key)
        .env("CODEBUDDY_AUTH_TOKEN", key)
        .env("CODEBUDDY_BASE_URL", base_url)
        .env("CODEBUDDY_MODEL", context.model)
        .env("CODEBUDDY_CONFIG_DIR", &directory)
        .env("CODEBUDDY_API_KEY_HELPER_DISABLED", "1");
    Ok(())
}

fn prepare_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    let launch = super::super::tool_replay_mcp::launch(context, tape)?;
    let program = launch.program.to_str().ok_or_else(|| {
        Error::Run("ASB local-tool replay executable path is not valid UTF-8".into())
    })?;
    let args = launch
        .args
        .iter()
        .map(|argument| {
            argument.to_str().map(str::to_owned).ok_or_else(|| {
                Error::Run("ASB local-tool replay arguments are not valid UTF-8".into())
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    // CodeBuddy accepts an inline MCP document.  Do not materialize this
    // inside the workload checkout: an untracked .mcp.json changes both the
    // agent trajectory and the grader's view of the repository.
    let configuration = serde_json::json!({
        "mcpServers": {
            "asb-local-tools": {
                "type": "stdio",
                "command": program,
                "args": args,
            }
        }
    });
    let configuration = serde_json::to_string(&configuration).map_err(|error| {
        Error::Run(format!(
            "cannot encode WorkBuddy MCP configuration: {error}"
        ))
    })?;
    context
        .command
        // No inherited CodeBuddy tools can bypass the sealed private MCP
        // track.  The selected server is the only local-tool authority.
        .arg("--tools")
        .arg("")
        .arg("--strict-mcp-config")
        .arg("--mcp-config")
        .arg(configuration);
    Ok(())
}

fn configure(directory: &std::path::Path, model: &str, base_url: &str) -> Result<(), Error> {
    std::fs::create_dir_all(directory).map_err(|error| {
        Error::Run(format!(
            "cannot create WorkBuddy provider configuration directory: {error}"
        ))
    })?;
    let configuration = serde_json::json!([{
        "id": model, "name": model, "url": base_url, "api": "openai-responses"
    }]);
    let content = serde_json::to_vec_pretty(&configuration).map_err(|error| {
        Error::Run(format!(
            "cannot render WorkBuddy provider configuration: {error}"
        ))
    })?;
    std::fs::write(directory.join("models.json"), content).map_err(|error| {
        Error::Run(format!(
            "cannot write WorkBuddy provider configuration: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentAdapter;

    #[test]
    fn adapter_reports_final_native_turns_without_counting_snapshots() {
        assert_eq!(
            ADAPTER.step_count(
                r#"[{"type":"result","num_turns":3},{"type":"result","num_turns":3}]"#,
                ""
            ),
            Some(3)
        );
        assert_eq!(
            ADAPTER.step_count(r#"{"type":"result","subtype":"error","num_turns":2}"#, ""),
            Some(2)
        );
        assert_eq!(
            ADAPTER.step_count(r#"{"type":"result","num_turns":-1}"#, ""),
            None
        );
        assert_eq!(ADAPTER.step_count(r#"{"num_turns":3}"#, ""), None);
    }

    #[test]
    fn provider_configuration_is_owned_by_the_workbuddy_adapter() {
        let directory =
            std::env::temp_dir().join(format!("asb-workbuddy-config-{}", std::process::id()));
        configure(&directory, "test-model", "http://127.0.0.1:3456/v1")
            .expect("configuration succeeds");
        let text =
            std::fs::read_to_string(directory.join("models.json")).expect("configuration exists");
        assert!(text.contains("test-model"));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn adapter_pins_the_documented_compaction_ceiling_for_replay_stability() {
        let config = crate::benchmark::BenchmarkConfig {
            planning: None,
            agent: "workbuddy".into(),
            workload: "bug-fix".into(),
            provider: "openrouter".into(),
            model: "test-model".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
        };
        let spec = command(&CommandContext {
            config: &config,
            endpoint: Some("http://127.0.0.1:3000/v1"),
            interface: Some(crate::endpoint::Interface::OpenAiResponses),
            prompt: "repair",
        });
        assert_eq!(
            spec.args
                .windows(2)
                .find(|pair| pair[0] == "--autocompact")
                .map(|pair| pair[1].as_str()),
            Some("1m")
        );
    }

    #[test]
    fn tape_backed_preparation_uses_inline_private_mcp_configuration() {
        let root =
            std::env::temp_dir().join(format!("asb-workbuddy-tool-replay-{}", std::process::id()));
        let mut command = std::process::Command::new("codebuddy");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiResponses,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("asb-replay".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let replay = root.join("replay.jsonl");
        let transcript = root.join("local-tools.jsonl");
        std::fs::write(&replay, "{}\n").unwrap();
        std::fs::write(&transcript, "{\"schema_version\":1,\"events\":[]}\n").unwrap();
        ADAPTER
            .prepare(&mut PreparationContext {
                command: &mut command,
                model: "test-model",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &root,
                run_dir: &root,
                tape: Some(crate::agents::TapeContext {
                    mode: crate::tape::Mode::Replay,
                    cassette: &replay,
                }),
            })
            .unwrap();
        let args = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(args.windows(2).any(|pair| pair == ["--tools", ""]));
        assert!(
            args.iter()
                .any(|argument| argument == "--strict-mcp-config")
        );
        let mcp = args
            .windows(2)
            .find(|pair| pair[0] == "--mcp-config")
            .map(|pair| serde_json::from_str::<serde_json::Value>(&pair[1]).unwrap())
            .expect("inline MCP configuration");
        assert_eq!(mcp["mcpServers"]["asb-local-tools"]["type"], "stdio");
        assert_eq!(
            mcp["mcpServers"]["asb-local-tools"]["args"][0],
            "__asb-tool-replay"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
