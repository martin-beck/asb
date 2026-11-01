use super::{CommandContext, CommandSpec, PreparationContext, PromptDelivery, StaticAdapter};
use crate::{Error, install::Recipe};

const MCP_PATH_BRIDGE: &str = include_str!("../../../assets/scripts/workbuddy-mcp-proxy.mjs");

// CodeBuddy restores these complete client defaults after resuming a
// conversation, although its initial request omits them. This adapter-owned
// opaque wire extension keeps that client-local resume state out of cassette
// identity only for WorkBuddy's exact envelope; arbitrary provider requests
// and partial explicit settings remain replay-significant.
const TAPE_WIRE_EXTENSION: &str = r#"
export function normalizeRequest(request) {
  const value = request?.body;
  if (!value || typeof value !== "object" || Array.isArray(value)
      || !Array.isArray(value.messages)) return value;
  const workBuddyMessage = value.messages.some((message) =>
    message && typeof message === "object" && !Array.isArray(message)
      && typeof message.agent === "string"
      && typeof message.conversationRequestId === "string",
  );
  const extraVars = value.extra_vars;
  const exactExtraVars = extraVars && typeof extraVars === "object" && !Array.isArray(extraVars)
    && Object.keys(extraVars).length === 1 && typeof extraVars.initiator === "string";
  const completeDefaults = typeof value.cache_retention === "string"
    && exactExtraVars
    && typeof value.max_tokens === "string"
    && typeof value.reasoning_effort === "string"
    && typeof value.reasoning_summary === "string"
    && typeof value.temperature === "number"
    && typeof value.verbosity === "string";
  if (!workBuddyMessage) return value;
  for (const message of value.messages) {
    if (!message || typeof message !== "object" || Array.isArray(message)) continue;
    // CodeBuddy regenerates these per-message continuation identifiers. They
    // are client bookkeeping, not model-visible message content or tool data.
    if (typeof message.conversationRequestId === "string") {
      message.conversationRequestId = "<asb-workbuddy-conversation-request>";
    }
    if (typeof message.messageId === "string") {
      message.messageId = "<asb-workbuddy-message>";
    }
    if (typeof message.entryId === "string") {
      message.entryId = "<asb-workbuddy-entry>";
    }
    if (typeof message.traceId === "string") {
      message.traceId = "<asb-workbuddy-trace>";
    }
  }
  if (!completeDefaults) return value;
  for (const key of [
    "cache_retention", "extra_vars", "max_tokens", "reasoning_effort",
    "reasoning_summary", "temperature", "verbosity",
  ]) delete value[key];
  return value;
}
"#;
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "workbuddy",
    "codebuddy",
    Recipe::Npm {
        // Pin the current CodeBuddy CLI release used by this adapter. ASB
        // preserves its sealed local-tool replay semantics independently.
        package: "@tencent-ai/codebuddy-code@2.178.1",
        executable: "codebuddy",
        manifest: None,
    },
    &[crate::endpoint::CHAT],
    command,
    prepare_adapter,
)
.with_estimated_disk_mib(768)
.with_complete_tool_replay(prepare_tool_replay)
.with_collection(collect_artifacts)
.with_artifact_specs(&[
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "local-tools.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "mcp-trace",
        filename: "workbuddy-mcp-metadata.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
])
.with_replay_credential_vars(&["CODEBUDDY_API_KEY", "CODEBUDDY_AUTH_TOKEN"])
.with_state_environment_vars(&["CODEBUDDY_CONFIG_DIR"])
.with_output_usage(output_usage)
.with_final_response(final_response)
.with_step_count(step_count)
.with_tape_wire_extension(TAPE_WIRE_EXTENSION);

fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    // Use CodeBuddy's own active-agent history counter from its final result.
    super::super::final_result_counter(stdout, "num_turns")
}

fn final_response(output: &str, _: &std::path::Path) -> Option<String> {
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
    let events = if let Ok(document) = serde_json::from_str::<serde_json::Value>(output) {
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
    if result.get("is_error").and_then(|v| v.as_bool()) != Some(false)
        || result.get("subtype").and_then(|v| v.as_str()) != Some("success")
    {
        return None;
    }
    result
        .get("result")
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .map(str::to_owned)
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
    fn native_event(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Array(events) => events.iter().any(native_event),
            serde_json::Value::Object(event) => matches!(
                event.get("type").and_then(|v| v.as_str()),
                Some("result" | "message")
            ),
            _ => false,
        }
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout)
        && native_event(&value)
    {
        // Do not parse task or tool contents if a native result has no usage.
        return result(&value).unwrap_or_default().with_derived_total();
    }
    crate::usage::usage_from_text(&format!("{stdout}\n{stderr}")).with_derived_total()
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
            // A tape run is an isolated attempt. Prevent CodeBuddy from
            // restoring or retaining a process-external conversation whose
            // post-tool continuation state is not part of the cassette.
            "--no-session-persistence".into(),
            "-y".into(),
        ],
        prompt_delivery: PromptDelivery::Argument,
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
    let launch = super::super::tool_replay_mcp::with_metadata_trace(
        super::super::tool_replay_mcp::launch(context, tape)?,
        &context
            .run_dir
            .join("mcp-trace/workbuddy-mcp-metadata.jsonl"),
    );
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
    let bridge = context.runtime_dir.join("workbuddy-mcp-proxy.mjs");
    std::fs::write(&bridge, MCP_PATH_BRIDGE)
        .map_err(|error| Error::Run(format!("cannot write WorkBuddy MCP path bridge: {error}")))?;
    let child = serde_json::to_string(
        &std::iter::once(program.to_owned())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>(),
    )
    .map_err(|_| Error::Run("cannot encode WorkBuddy MCP child command".into()))?;
    // CodeBuddy accepts an inline MCP document.  Do not materialize this
    // inside the workload checkout: an untracked .mcp.json changes both the
    // agent trajectory and the grader's view of the repository.
    let configuration = serde_json::json!({
        "mcpServers": {
            "asb-local-tools": {
                "type": "stdio",
                "command": "node",
                "args": [bridge, child],
                // CodeBuddy defers MCP discovery by default.  A benchmark's
                // first model turn must see the sealed tool set, rather than
                // completing before the server is advertised.
                "alwaysLoad": true,
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

fn collect_artifacts(
    context: &super::super::CollectionContext<'_>,
) -> super::super::AgentArtifacts {
    let mut artifacts = super::super::AgentArtifacts::default();
    if let Some(bytes) = context.frozen.bytes("workbuddy-mcp-metadata.jsonl") {
        artifacts
            .sealed_artifacts
            .push(super::super::SealedArtifact {
                kind: "workbuddy-mcp-metadata".into(),
                bytes: bytes.to_vec(),
            });
    }
    artifacts
}

fn configure(directory: &std::path::Path, model: &str, base_url: &str) -> Result<(), Error> {
    std::fs::create_dir_all(directory).map_err(|error| {
        Error::Run(format!(
            "cannot create WorkBuddy provider configuration directory: {error}"
        ))
    })?;
    let configuration = serde_json::json!([{
        "id": model, "name": model, "url": base_url, "api": "openai"
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
    #[test]
    fn failed_result_does_not_grade_intermediate_assistant_output() {
        let output = r#"[{"type":"assistant","content":"intermediate"},{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["failed"]}]"#;
        use crate::agents::AgentAdapter;
        let path = std::path::Path::new(".");
        assert!(super::ADAPTER.final_response(output, path).is_none());
        let success =
            r#"[{"type":"result","subtype":"success","is_error":false,"result":"answer"}]"#;
        assert_eq!(
            super::ADAPTER.final_response(success, path).as_deref(),
            Some("answer")
        );
        for value in [
            r#"[{"type":"assistant","content":"intermediate"}]"#,
            r#"[{"type":"result","subtype":"success","result":"missing flag"}]"#,
            r#"[{"type":"result","is_error":false,"result":"missing subtype"}]"#,
            r#"[{"type":"result","subtype":"success","is_error":false,"result":""}]"#,
            r#"[{"type":"result","subtype":"success","is_error":false,"result":"answer"},{"type":"result","is_error":true}]"#,
        ] {
            assert!(
                super::ADAPTER.final_response(value, path).is_none(),
                "{value}"
            );
        }
    }

    #[test]
    fn result_without_usage_does_not_count_task_payload() {
        let output =
            r#"{"type":"result","result":{"usage":{"input_tokens":999,"output_tokens":888}}}"#;
        assert!(!super::output_usage(output, "", false).any());
    }

    use super::*;
    use crate::agents::AgentAdapter;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::process::{Command, Stdio};

    fn replay_request(session: &crate::tape::Session, body: &str) -> String {
        let address = session
            .url()
            .trim_start_matches("http://")
            .trim_end_matches("/v1");
        let mut socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        write!(
            socket,
            "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn tape_resume_defaults_are_owned_by_workbuddy_extension() {
        let root = std::env::temp_dir().join(format!(
            "asb-workbuddy-tape-defaults-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let cassette = root.join("record.jsonl");
        let recorded = r#"{"model":"m","messages":[{"role":"assistant","agent":"workbuddy","conversationRequestId":"0123456789abcdef0123456789abcdef","messageId":"0123456789abcdef0123456789abcdef","entryId":"0123456789abcdef0123456789abcdef","traceId":"0123456789abcdef0123456789abcdef","content":"work"}],"stream":true,"stream_options":{"include_usage":true},"tools":[{"type":"function"}]}"#;
        let replayed = r#"{"model":"m","messages":[{"role":"assistant","agent":"workbuddy","conversationRequestId":"abcdef0123456789abcdef0123456789","messageId":"abcdef0123456789abcdef0123456789","entryId":"abcdef0123456789abcdef0123456789","traceId":"abcdef0123456789abcdef0123456789","content":"work"}],"stream":true,"stream_options":{"include_usage":true},"tools":[{"type":"function"}],"cache_retention":"default","extra_vars":{"initiator":"workbuddy"},"max_tokens":"8192","reasoning_effort":"medium","reasoning_summary":"auto","temperature":0,"verbosity":"normal"}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body":recorded},
            "response": {"status":200, "headers":{"content-type":"application/json"}, "body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let session = crate::tape::Session::start_for_agent(
            crate::tape::Mode::Replay,
            &cassette,
            None,
            false,
            &root,
            Some("workbuddy"),
        )
        .unwrap();
        assert!(replay_request(&session, replayed).contains("200 OK"));
        session.stop().unwrap();

        let strict = crate::tape::Session::start_for_agent(
            crate::tape::Mode::Replay,
            &cassette,
            None,
            false,
            &root,
            Some("opencode"),
        )
        .unwrap();
        assert!(replay_request(&strict, replayed).contains("409 Conflict"));
        assert!(strict.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

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
        assert!(text.contains("\"api\": \"openai\""));
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
            retention: Default::default(),
            mcp_servers: Vec::new(),
        };
        let spec = command(&CommandContext {
            executable: "workbuddy",
            config: &config,
            endpoint: Some("http://127.0.0.1:3000/v1"),
            interface: Some(crate::endpoint::Interface::OpenAiResponses),
            prompt: "repair",
            max_steps: None,
            native_tool_mode: None,
            native_tool: None,
        });
        assert_eq!(
            spec.args
                .windows(2)
                .find(|pair| pair[0] == "--autocompact")
                .map(|pair| pair[1].as_str()),
            Some("1m")
        );
        assert!(
            spec.args
                .iter()
                .any(|argument| argument == "--no-session-persistence")
        );
    }

    #[test]
    fn tape_backed_preparation_uses_inline_private_mcp_configuration() {
        let root =
            std::env::temp_dir().join(format!("asb-workbuddy-tool-replay-{}", std::process::id()));
        let mut command = std::process::Command::new("codebuddy");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("asb-replay".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let replay = root.join("replay.jsonl");
        let transcript = root.join("local-tools.jsonl");
        std::fs::write(&replay, "{}\n").unwrap();
        std::fs::write(
            &transcript,
            crate::agents::tool_replay::Transcript::default()
                .encode()
                .unwrap(),
        )
        .unwrap();
        ADAPTER
            .prepare(&mut PreparationContext {
                command: &mut command,
                model: "test-model",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &root,
                run_dir: &root,
                mcp_servers: &[],
                mcp_tape: None,
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
            mcp["mcpServers"]["asb-local-tools"]["alwaysLoad"], true,
            "the first headless turn must wait for the sealed MCP server"
        );
        assert_eq!(mcp["mcpServers"]["asb-local-tools"]["command"], "node");
        assert!(
            mcp["mcpServers"]["asb-local-tools"]["args"][0]
                .as_str()
                .is_some_and(|value| value.ends_with("workbuddy-mcp-proxy.mjs"))
        );
        assert!(
            mcp["mcpServers"]["asb-local-tools"]["args"][1]
                .as_str()
                .is_some_and(|value| value.contains("__asb-tool-replay"))
        );
        let child: Vec<String> = serde_json::from_str(
            mcp["mcpServers"]["asb-local-tools"]["args"][1]
                .as_str()
                .expect("encoded private MCP child"),
        )
        .expect("private MCP child array");
        assert!(child.iter().any(|argument| argument == "--metadata-trace"));
        assert!(
            child
                .iter()
                .any(|argument| { argument.ends_with("mcp-trace/workbuddy-mcp-metadata.jsonl") })
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn private_mcp_bridge_owns_codebuddy_testbed_path_translation() {
        assert!(MCP_PATH_BRIDGE.contains("/testbed"));
        assert!(MCP_PATH_BRIDGE.contains("relativeTestbed"));
        assert!(MCP_PATH_BRIDGE.contains("tools/call"));
    }

    #[test]
    fn private_mcp_bridge_maps_testbed_and_rejects_traversal() {
        let root = std::env::temp_dir().join(format!(
            "asb-workbuddy-mcp-bridge-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let bridge = root.join("bridge.mjs");
        let echo = root.join("echo.mjs");
        std::fs::write(&bridge, MCP_PATH_BRIDGE).unwrap();
        std::fs::write(&echo, "process.stdin.pipe(process.stdout);\n").unwrap();
        let child =
            serde_json::to_string(&vec!["node".to_owned(), echo.display().to_string()]).unwrap();
        let mut process = Command::new("node")
            .arg(&bridge)
            .arg(child)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let requests = [
            serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"/testbed/src/parser.py"}}}),
            serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"/etc/passwd"}}}),
            serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"/testbed/../secret"}}}),
        ];
        {
            let input = process.stdin.as_mut().unwrap();
            for request in requests {
                writeln!(input, "{request}").unwrap();
            }
        }
        let output = process.wait_with_output().unwrap();
        assert!(output.status.success());
        let frames = std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 3);
        let frame = |id| frames.iter().find(|frame| frame["id"] == id).unwrap();
        assert_eq!(frame(1)["params"]["arguments"]["path"], "src/parser.py");
        assert_eq!(frame(2)["params"]["arguments"]["path"], "/etc/passwd");
        assert_eq!(frame(3)["error"]["code"], -32602);
        let _ = std::fs::remove_dir_all(root);
    }
}
