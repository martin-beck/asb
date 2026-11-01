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
.with_estimated_disk_mib(1024)
.with_complete_tool_replay(prepare_tool_replay)
.with_replay_credential_vars(&["CONTINUE_API_KEY"])
.with_state_environment_vars(&["CONTINUE_CONFIG", "CONTINUE_GLOBAL_DIR"])
.with_artifact_specs(&[
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "local-tools.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "continue",
        filename: "session.json",
        format: super::super::ArtifactFormat::JsonSingleton {
            directories: &["config", "sessions"],
            ignored_name: "sessions.json",
            project: project_session,
        },
    },
])
.with_collection(collect)
.with_output_usage(output_usage)
.with_final_response(final_response)
.with_artifact_final_response(final_response_with_session)
.with_guest_artifact_answer(validate_guest_answer);

fn compaction_status(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let status = object.get("status").and_then(|v| v.as_str());
    let message = object.get("message").and_then(|v| v.as_str());
    match (status, message) {
        (Some("info"), Some("Auto-compacting triggered")) => {
            object.len() == 3
                && object
                    .get("contextUsage")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s.ends_with('%'))
        }
        (Some("success"), Some("Auto-compacted successfully"))
        | (Some("warning"), Some("Auto-compaction failed, continuing without compaction")) => {
            object.len() == 2
        }
        _ => false,
    }
}

// Continue emits optional auto-compaction status objects before its final
// JSON value. Reject ambiguous status-only output rather than grade it.
fn final_response(output: &str, _: &std::path::Path) -> Option<String> {
    let mut stream = serde_json::Deserializer::from_str(output).into_iter::<serde_json::Value>();
    let mut previous_end = 0;
    let mut result = None;
    while let Some(value) = stream.next() {
        let value = value.ok()?;
        let end = stream.byte_offset();
        let raw = output.get(previous_end..end)?.trim();
        previous_end = end;
        if result.is_some() {
            return None;
        }
        if compaction_status(&value) {
            continue;
        }
        result = match value {
            serde_json::Value::String(answer) if !answer.trim().is_empty() => Some(answer),
            serde_json::Value::String(_) | serde_json::Value::Null => return None,
            _ => Some(raw.to_owned()),
        };
    }
    result
}

// Preserve only identity, assistant usage and the final message needed for
// cross-checking; do not copy prompts or tool transcripts into evidence.
fn project_session(value: &serde_json::Value) -> Option<serde_json::Value> {
    let id = value.get("sessionId")?.as_str()?;
    let history = value.get("history")?.as_array()?;
    if history.len() > 4096 {
        return None;
    }
    let mut reduced = Vec::with_capacity(history.len());
    for (i, item) in history.iter().enumerate() {
        let message = item.get("message")?;
        let role = message.get("role")?.as_str()?;
        let mut selected = serde_json::Map::new();
        selected.insert("role".into(), role.into());
        if role == "assistant"
            && let Some(usage) = message.get("usage")
        {
            selected.insert("usage".into(), usage.clone());
        }
        if i + 1 == history.len() && role == "assistant" {
            if let Some(content) = message.get("content") {
                selected.insert("content".into(), content.clone());
            }
            if let Some(calls) = message.get("toolCalls") {
                let pending = calls.as_array().is_none_or(|items| !items.is_empty());
                selected.insert(
                    "toolCalls".into(),
                    if pending {
                        serde_json::json!([null])
                    } else {
                        serde_json::json!([])
                    },
                );
            }
        }
        reduced.push(serde_json::json!({"message":selected}));
    }
    Some(serde_json::json!({"sessionId":id,"history":reduced}))
}

fn validated_session(frozen: &super::super::FrozenArtifacts) -> Option<&serde_json::Value> {
    let snapshot = frozen.document("session.json")?;
    let filename = snapshot.get("filename")?.as_str()?;
    let doc = snapshot.get("document")?;
    let id = doc.get("sessionId")?.as_str()?;
    if id.len() != 36
        || !id.bytes().enumerate().all(|(i, byte)| {
            if [8, 13, 18, 23].contains(&i) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        || filename.strip_suffix(".json") != Some(id)
    {
        return None;
    }
    Some(doc)
}

fn final_response_with_session(
    stdout: &str,
    frozen: &super::super::FrozenArtifacts,
    _: &std::path::Path,
    complete: bool,
) -> Option<String> {
    if !complete {
        return None;
    }
    verified_session_answer(stdout, validated_session(frozen)?)
}

fn validate_guest_answer(answer: &str, frozen: &super::super::FrozenArtifacts) -> bool {
    session_raw_answer(validated_session(frozen)).is_some_and(|raw| raw == answer)
}

fn session_raw_answer(session: Option<&serde_json::Value>) -> Option<&str> {
    let message = session?
        .get("history")?
        .as_array()?
        .last()?
        .get("message")?;
    if message.get("role")?.as_str()? != "assistant" {
        return None;
    }
    if message
        .get("toolCalls")
        .is_some_and(|calls| calls.as_array().is_none_or(|items| !items.is_empty()))
    {
        return None;
    }
    let raw = message.get("content")?.as_str()?.trim();
    (!raw.is_empty()).then_some(raw)
}

/// Cross-check Continue's emitted value against the final tool-free session
/// message before using the raw answer. Absence or disagreement is unknown.
fn verified_session_answer(stdout: &str, session: &serde_json::Value) -> Option<String> {
    let raw = session_raw_answer(Some(session))?;
    let mut pending = None;
    let mut count = 0usize;
    for value in serde_json::Deserializer::from_str(stdout).into_iter::<serde_json::Value>() {
        let value = value.ok()?;
        if let Some(previous) = pending.replace(value)
            && !compaction_status(&previous)
        {
            return None;
        }
        count += 1;
        if count > 1024 {
            return None;
        }
    }
    let actual = pending?;
    let expected = serde_json::from_str::<serde_json::Value>(raw).unwrap_or_else(|_| serde_json::json!({"response":raw,"status":"success","note":"Response was not valid JSON, so it was wrapped in a JSON object"}));
    let matches = actual == expected;
    matches.then(|| raw.to_owned())
}

// Headless JSON output is the model's answer, not a usage event. Usage comes
// from the private session's assistant messages or a provider cassette.
fn output_usage(_: &str, _: &str, _: bool) -> crate::measurements::TokenUsage {
    Default::default()
}

const BUILT_IN_TOOLS: &[&str] = &[
    "AskQuestion",
    "Edit",
    "Exit",
    "Fetch",
    "List",
    "MultiEdit",
    "Read",
    "ReportFailure",
    "Bash",
    "Search",
    "Status",
    "Subagent",
    "Skills",
    "UploadArtifact",
    "Diff",
    "Checklist",
    "Write",
];
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
    let local_tools = context
        .command
        .get_envs()
        .find_map(|(name, value)| (name == "ASB_CONTINUE_TOOL_REPLAY_COMMAND").then_some(value))
        .flatten()
        .map(|value| value.to_string_lossy().into_owned());
    context
        .command
        .env_remove("ASB_CONTINUE_TOOL_REPLAY_COMMAND");
    configure(
        &directory,
        provider,
        context.model,
        url,
        "ASB_AGENT_API_KEY",
        local_tools.as_deref(),
    )?;
    context
        .command
        .env_remove("CONTINUE_CLI_TEST")
        .env_remove("CONTINUE_CLI_TEST_SESSION_ID")
        .env("CONTINUE_CONFIG", directory.join("config.yaml"))
        .env("CONTINUE_GLOBAL_DIR", &directory)
        .env("CONTINUE_API_KEY", key)
        .env("ASB_AGENT_API_KEY", key)
        .env("CONTINUE_API_BASE", url)
        .env("CONTINUE_MODEL", context.model);
    Ok(())
}

/// Continue exposes its native filesystem and shell operations as a fixed
/// built-in tool set. Tape-backed attempts exclude that set and configure the
/// private ASB MCP server as the only local-action authority. This is adapter
/// specific: the runner sees only the sealed transcript sidecar.
fn prepare_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    let launch = super::super::tool_replay_mcp::launch(context, tape)?;
    let command = std::iter::once(launch.program.as_os_str())
        .chain(launch.args.iter().map(std::ffi::OsString::as_os_str))
        .map(|part| {
            part.to_str().map(str::to_owned).ok_or_else(|| {
                Error::Run("Continue local-tool command contains non-Unicode path data".into())
            })
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("\u{1f}");
    context
        .command
        .env("ASB_CONTINUE_TOOL_REPLAY_COMMAND", command);
    for tool in BUILT_IN_TOOLS {
        context.command.arg("--exclude").arg(tool);
    }
    Ok(())
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let Some(value) = validated_session(context.frozen) else {
        return AgentArtifacts::default();
    };
    AgentArtifacts {
        token_usage: Some(session_usage(
            value,
            context.interface == Some(crate::endpoint::Interface::AnthropicMessages)
                && !context.anthropic_input_includes_cache,
        )),
        steps: session_steps(value),
        final_response: None,
        ..Default::default()
    }
}

/// Continue persists one provider usage object per completed assistant turn.
/// Treat a missing assistant usage object as incomplete evidence rather than
/// reporting a partial step count.
fn session_steps(value: &serde_json::Value) -> Option<u64> {
    let history = value.get("history")?.as_array()?;
    let mut steps = 0_u64;
    for item in history {
        let message = item.get("message")?;
        if message.get("role").and_then(|value| value.as_str()) != Some("assistant") {
            continue;
        }
        message.get("usage")?;
        steps = steps.checked_add(1)?;
    }
    (steps > 0).then_some(steps)
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
    local_tools: Option<&str>,
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
    let mut configuration = format!(
        "name: ASB provider\nversion: 1.0.0\nmodels:\n  - name: {model}\n    provider: {provider}\n    model: {model}\n    apiBase: {base_url}\n    apiKey: ${{{{ secrets.//{api_key_env} }}}}\n"
    );
    if let Some(command) = local_tools {
        let mut command = command.split('\u{1f}');
        let program = command
            .next()
            .filter(|program| !program.is_empty())
            .ok_or_else(|| Error::Run("Continue local-tool command is empty".into()))?;
        let program = serde_json::to_string(program)
            .map_err(|error| Error::Run(format!("cannot render Continue MCP command: {error}")))?;
        configuration
            .push_str("mcpServers:\n  - name: asb_local_tools\n    type: stdio\n    command: ");
        configuration.push_str(&program);
        configuration.push_str("\n    args:\n");
        for argument in command {
            let argument = serde_json::to_string(argument).map_err(|error| {
                Error::Run(format!("cannot render Continue MCP argument: {error}"))
            })?;
            configuration.push_str("      - ");
            configuration.push_str(&argument);
            configuration.push('\n');
        }
    }
    std::fs::write(directory.join("config.yaml"), configuration).map_err(|error| {
        Error::Run(format!(
            "cannot write Continue provider configuration: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn singleton_session_freeze_and_guest_transfer_keep_only_needed_data() {
        use crate::agents::AgentAdapter;
        for mode in ["missing", "multiple", "valid", "long", "oversized"] {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/continue-singleton-tests")
                .join(crate::workloads::TaskIdentity::local("continue", None).attempt_id);
            std::fs::create_dir_all(&root).unwrap();
            let mut owner =
                crate::agents::ArtifactOwner::prepare(&root, super::ADAPTER.artifact_specs())
                    .unwrap();
            let sessions = root.join("continue/config/sessions");
            assert!(sessions.is_dir());
            if mode != "missing" {
                let id = "00112233-4455-6677-8899-aabbccddeeff";
                let prompt = if mode == "long" {
                    "SECRET_PROMPT".repeat(100_000)
                } else {
                    "SECRET_PROMPT".to_owned()
                };
                let doc = serde_json::json!({"sessionId":id,"history":[
                    {"message":{"role":"user","content":prompt}},
                    {"message":{"role":"assistant","content":"intermediate","toolCalls":[{"secret":"SECRET_TOOL"}]}},
                    {"message":{"role":"assistant","content":"answer","usage":{"inputTokens":4}}}
                ]});
                std::fs::write(sessions.join(format!("{id}.json")), doc.to_string()).unwrap();
                if mode == "oversized" {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(sessions.join(format!("{id}.json")))
                        .unwrap()
                        .set_len(16 * 1024 * 1024 + 1)
                        .unwrap();
                }
                if mode == "multiple" {
                    std::fs::write(sessions.join("other.json"), "{}").unwrap();
                }
            }
            let frozen = owner.freeze();
            assert_eq!(frozen.error, mode == "oversized");
            if mode == "oversized" {
                assert!(frozen.diagnostic().unwrap().contains("16 MiB"));
            }
            assert_eq!(
                frozen.document("session.json").is_some(),
                mode == "valid" || mode == "long"
            );
            let encoded = frozen.deferred_collection_bytes().unwrap();
            let text = String::from_utf8_lossy(&encoded);
            assert!(!text.contains("SECRET_PROMPT") && !text.contains("SECRET_TOOL"));
            let restored =
                crate::agents::FrozenArtifacts::from_deferred_collection_bytes(&encoded).unwrap();
            if mode == "valid" || mode == "long" {
                assert_eq!(
                    super::session_raw_answer(super::validated_session(&restored)),
                    Some("answer")
                );
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn session_cross_check_returns_only_raw_matching_final_answer() {
        let stdout = r#"{"response":"answer","status":"success","note":"Response was not valid JSON, so it was wrapped in a JSON object"}"#;
        let doc = serde_json::json!({"history":[{"message":{"role":"user","content":"prompt"}},{"message":{"role":"assistant","content":"answer"}}]});
        assert_eq!(
            super::verified_session_answer(stdout, &doc).as_deref(),
            Some("answer")
        );
        let status = r#"{"status":"success","message":"Auto-compacted successfully"}"#;
        let status_doc =
            serde_json::json!({"history":[{"message":{"role":"assistant","content":status}}]});
        assert_eq!(
            super::verified_session_answer(status, &status_doc).as_deref(),
            Some(status)
        );
        let null_doc =
            serde_json::json!({"history":[{"message":{"role":"assistant","content":"null"}}]});
        assert_eq!(
            super::verified_session_answer("null", &null_doc).as_deref(),
            Some("null")
        );
        assert!(
            !super::verified_session_answer(stdout, &doc)
                .unwrap()
                .contains("success")
        );
        for doc in [
            serde_json::json!({"history":[]}),
            serde_json::json!({"history":[{"message":{"role":"assistant","content":"different"}}]}),
            serde_json::json!({"history":[{"message":{"role":"user","content":"answer"}}]}),
            serde_json::json!({"history":[{"message":{"role":"assistant","content":"answer","toolCalls":[{}]}}]}),
        ] {
            assert!(super::verified_session_answer(stdout, &doc).is_none());
        }
        let json = serde_json::json!({"history":[{"message":{"role":"assistant","content":"{\"key\": 2}"}}]});
        assert_eq!(
            super::verified_session_answer(r#"{"key":2}"#, &json).as_deref(),
            Some("{\"key\": 2}")
        );
    }

    #[test]
    fn session_check_consumes_complete_large_stdout() {
        let doc =
            serde_json::json!({"history":[{"message":{"role":"assistant","content":"answer"}}]});
        let status = serde_json::json!({"status":"info","message":"Auto-compacting triggered","contextUsage":format!("{}%", "0".repeat(1_050_000))});
        let stdout = format!(
            "{status}\n{}",
            serde_json::json!({"response":"answer","status":"success","note":"Response was not valid JSON, so it was wrapped in a JSON object"})
        );
        assert_eq!(
            super::verified_session_answer(&stdout, &doc).as_deref(),
            Some("answer")
        );
        assert!(super::verified_session_answer(&format!("{stdout}\n\"extra\""), &doc).is_none());
    }

    #[test]
    fn json_answer_is_available_for_grading() {
        use crate::agents::AgentAdapter;
        let output = r#"{"response":"answer", "status":"success"}"#;
        let path = std::path::Path::new(".");
        assert_eq!(
            super::ADAPTER.final_response(output, path).as_deref(),
            Some(output)
        );
        let json = r#"{"arbitrary":[1,2],"role":"tool"}"#;
        assert_eq!(
            super::ADAPTER.final_response(json, path).as_deref(),
            Some(json)
        );
        assert_eq!(
            super::ADAPTER
                .final_response(r#""plain answer""#, path)
                .as_deref(),
            Some("plain answer")
        );
        let status =
            r#"{"status":"info","message":"Auto-compacting triggered","contextUsage":"95%"}"#;
        let done = r#"{"status":"success","message":"Auto-compacted successfully"}"#;
        assert!(super::ADAPTER.final_response(status, path).is_none());
        assert_eq!(
            super::ADAPTER
                .final_response(&format!("{status}\n{done}\n{output}"), path)
                .as_deref(),
            Some(output)
        );
        assert!(
            super::ADAPTER
                .final_response(&format!("{output}\n{done}"), path)
                .is_none()
        );
        for invalid in ["", "null", "not JSON", "{\"response\":"] {
            assert!(super::ADAPTER.final_response(invalid, path).is_none());
        }
    }

    #[test]
    fn json_answer_is_not_usage_when_session_is_missing() {
        let answer = r#"{"usage":{"input_tokens":999,"output_tokens":888}}"#;
        let output = crate::usage::from_sources("openrouter", "continue", answer, "", None);
        let usage = crate::usage::with_artifact_usage(output, None);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (None, None, None)
        );
    }
    use super::*;
    use crate::agents::AgentAdapter;

    #[test]
    fn tape_backed_preparation_replaces_continue_built_ins_with_the_sealed_mcp() {
        let root = std::env::temp_dir().join(format!(
            "asb-continue-tool-replay-{}-{}",
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
        let mut command = std::process::Command::new("cn");
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
        let config = std::fs::read_to_string(run_dir.join("continue/config/config.yaml")).unwrap();
        assert!(config.contains("mcpServers:"));
        assert!(config.contains("- name: asb_local_tools"));
        assert!(config.contains("__asb-tool-replay"));
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        for tool in BUILT_IN_TOOLS {
            assert!(
                arguments
                    .windows(2)
                    .any(|pair| { pair[0] == "--exclude" && pair[1] == *tool })
            );
        }
        assert!(run_dir.join("tool-replay/local-tools.jsonl").is_file());
        let _ = std::fs::remove_dir_all(root);
    }

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
        assert_eq!(session_steps(&document), Some(2));
        assert_eq!(session_steps(&incomplete), None);
    }

    #[test]
    fn anthropic_separate_cache_is_included_only_when_needed() {
        let doc = serde_json::json!({"history":[{"message":{"role":"assistant","usage":{"prompt_tokens":10,"completion_tokens":3,"prompt_tokens_details":{"cache_read_tokens":4,"cache_write_tokens":2}}}}]});
        assert_eq!(session_usage(&doc, true).total_tokens, Some(19));
        assert_eq!(session_usage(&doc, false).total_tokens, Some(13));
    }

    #[test]
    fn collection_uses_validated_frozen_session() {
        let id = "12345678-1234-1234-1234-123456789abc";
        let snapshot = serde_json::json!({"filename":format!("{id}.json"),"document":{"sessionId":id,"history":[{"message":{"role":"assistant","usage":{"prompt_tokens":10,"completion_tokens":3}}}]}});
        let frozen = super::super::super::FrozenArtifacts::for_test("session.json", snapshot);
        let context = CollectionContext {
            frozen: &frozen,
            provider_capture: None,
            run_dir: std::path::Path::new("."),
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
        };
        assert_eq!(
            collect(&context).token_usage.unwrap().total_tokens,
            Some(13)
        );
        let missing = super::super::super::FrozenArtifacts::default();
        let context = CollectionContext {
            frozen: &missing,
            ..context
        };
        assert!(collect(&context).token_usage.is_none());
        let bad = super::super::super::FrozenArtifacts::for_test(
            "session.json",
            serde_json::json!({"filename":"other.json","document":{"sessionId":id,"history":[]}}),
        );
        let context = CollectionContext {
            frozen: &bad,
            ..context
        };
        assert!(collect(&context).token_usage.is_none());
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
            None,
        )
        .expect("configuration succeeds");
        let text =
            std::fs::read_to_string(directory.join("config.yaml")).expect("configuration exists");
        assert!(text.contains("test-model"));
        assert!(text.contains("CLI2KEY_API_KEY"));
        let _ = std::fs::remove_dir_all(directory);
    }
}
