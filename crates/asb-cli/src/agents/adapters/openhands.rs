use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, PreparationContext,
    StaticAdapter,
};
use crate::{Error, install::Recipe};
/// Dormant project-local SDK seam for future native tool cassette transport.
///
/// The normal OpenHands command remains the installed CLI.  It exposes no
/// terminal-executor configuration, so no existing run can select this helper.
/// A future ASB-owned launcher may import the helper and provide a typed SDK
/// `ToolExecutor` once the bounded native-cassette schema is available.
const SDK_LAUNCHER: &str = include_str!("../../../assets/scripts/openhands-sdk-launcher.py");
const ARTIFACTS: &[super::super::ArtifactSpec] = &[super::super::ArtifactSpec {
    namespace: "openhands",
    filename: "native-tool-sidecar.jsonl",
    format: super::super::ArtifactFormat::JsonLines,
}];
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
.with_complete_tool_replay(prepare_native_tool_replay)
.with_native_tool_transport()
.with_artifact_specs(ARTIFACTS)
.with_collection(collect)
.with_bridge_entry(super::openhands_native::entry)
.with_bridge_aliases(&["__asb-openhands-native-bridge"])
.with_output_usage(output_usage)
.with_final_response(final_response);

// The CLI emits individual events, including arbitrary observation content;
// per-call usage comes from conversation state or the provider cassette.
fn output_usage(_: &str, _: &str, _: bool) -> crate::measurements::TokenUsage {
    Default::default()
}
// OpenHands headless JSON is the SDK event stream. Its finish action and
// agent MessageEvent are the SDK's final-response sources.
fn final_response(output: &str, _: &std::path::Path) -> Option<String> {
    let events = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    for event in events.iter().rev() {
        // Native replay's result follows SDK events and supersedes them,
        // including when its answer is null or missing.
        if event.get("type").and_then(|v| v.as_str()) == Some("result") {
            return event
                .get("text")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned);
        }
        let kind = event.get("kind").and_then(|v| v.as_str());
        let source = event.get("source").and_then(|v| v.as_str());
        if matches!(
            kind,
            Some("ConversationErrorEvent" | "UserRejectObservation" | "AgentErrorEvent")
        ) || (kind == Some("MessageEvent")
            && (source == Some("user")
                || event.pointer("/llm_message/role").and_then(|v| v.as_str()) == Some("user")))
        {
            return None;
        }
        if source != Some("agent") {
            continue;
        }
        match kind {
            Some("ActionEvent") => {
                if event.get("tool_name").and_then(|v| v.as_str()) != Some("finish")
                    || event.pointer("/action/kind").and_then(|v| v.as_str())
                        != Some("FinishAction")
                {
                    return None;
                }
                return event
                    .pointer("/action/message")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_owned);
            }
            Some("MessageEvent") => {
                if event.pointer("/llm_message/role").and_then(|v| v.as_str()) != Some("assistant")
                {
                    return None;
                }
                let parts = event.pointer("/llm_message/content")?.as_array()?;
                let answer = parts
                    .iter()
                    .filter(|part| part.get("type").and_then(|v| v.as_str()) == Some("text"))
                    .filter_map(|part| part.get("text").and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
                    .join("");
                return (!answer.trim().is_empty()).then_some(answer);
            }
            _ => {}
        }
    }
    None
}

fn command(context: &CommandContext<'_>) -> CommandSpec {
    if let Some(mode) = context.native_tool_mode {
        let native = context.native_tool;
        let launch_json = native.map(|native| {
            serde_json::to_string(native.launch).expect("native launch descriptor is serializable")
        });
        return CommandSpec {
            program: sdk_python(context.executable).unwrap_or_else(|| context.executable.into()),
            args: vec![
                "-c".into(),
                SDK_LAUNCHER.into(),
                "--mode".into(),
                match mode {
                    crate::tape::Mode::Record => "record",
                    crate::tape::Mode::Replay => "replay",
                }
                .into(),
                "--sidecar".into(),
                native.map_or_else(
                    || "".into(),
                    |native| native.path.to_string_lossy().into_owned(),
                ),
                "--launch-json".into(),
                launch_json.unwrap_or_default(),
                "--task".into(),
                context.prompt.into(),
            ],
            prompt_delivery: super::PromptDelivery::Embedded,
        };
    }
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
fn prepare_native_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    let output = context.run_dir.join("openhands");
    // `openhands` is a declared artifact namespace. ArtifactOwner claims it
    // before this hook runs, so preparation must not create it: the SDK
    // bridge creates its sidecar only after that claim has been established.
    if tape.mode == crate::tape::Mode::Record {
        context.command.env(
            "ASB_OPENHANDS_NATIVE_RECORD_SIDECAR",
            output.join("native-tool-sidecar.jsonl"),
        );
    }
    Ok(())
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

/// Project-local pip entry points have an absolute interpreter shebang. The
/// SDK launcher must use that interpreter, never a host Python that might not
/// contain the installed OpenHands SDK.
fn sdk_python(executable: &str) -> Option<String> {
    let line = std::fs::read_to_string(executable)
        .ok()?
        .lines()
        .next()?
        .to_owned();
    let interpreter = line.strip_prefix("#!")?.trim();
    (!interpreter.is_empty() && std::path::Path::new(interpreter).is_file())
        .then(|| interpreter.to_owned())
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let directory = context.run_dir.join("openhands/conversations");
    let mut artifacts = AgentArtifacts::default();
    let state = std::fs::read_dir(directory).ok().and_then(|entries| {
        let mut state = None;
        for entry in entries {
            let entry = entry.ok()?;
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if state.is_some() {
                return None;
            }
            state = Some(entry.path().join("base_state.json"));
        }
        state
    });
    if let Some(value) = state.as_deref().and_then(super::super::read_usage_document) {
        // OpenHands/LiteLLM reports cache reads alongside an inclusive input
        // total for providers such as cli2key. This accounting rule is owned
        // by the provider, not by the particular transport selected here.
        artifacts.token_usage = Some(conversation_usage(
            &value,
            context.anthropic_input_includes_cache,
        ));
        artifacts.steps = conversation_steps(&value);
    }
    if let Some(sidecar) = context.frozen.bytes("native-tool-sidecar.jsonl") {
        artifacts.native_tool_sidecar = Some(sidecar.to_vec());
    }
    artifacts
}

/// OpenHands records one token-usage item for each completed LiteLLM call.
/// Keep this aligned with `conversation_usage`: malformed parallel metric
/// arrays yield no count instead of a partial one.
fn conversation_steps(value: &serde_json::Value) -> Option<u64> {
    let metrics = value.pointer("/stats/usage_to_metrics")?.as_object()?;
    let mut steps = 0_u64;
    for metric in metrics.values() {
        let calls = metric.get("token_usages")?.as_array()?;
        if metric
            .get("response_latencies")
            .and_then(|value| value.as_array())
            .is_some_and(|latencies| latencies.len() != calls.len())
        {
            return None;
        }
        steps = steps.checked_add(u64::try_from(calls.len()).ok()?)?;
    }
    (steps > 0).then_some(steps)
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
    #[test]
    fn finish_action_message_is_final_answer() {
        use crate::agents::AgentAdapter;
        let event = r#"{"kind":"ActionEvent","source":"agent","tool_name":"finish","action":{"kind":"FinishAction","message":"answer"}}"#;
        let path = std::path::Path::new(".");
        assert_eq!(
            super::ADAPTER.final_response(event, path).as_deref(),
            Some("answer")
        );
        let text = r#"{"kind":"MessageEvent","source":"agent","llm_message":{"role":"assistant","content":[{"type":"text","text":"plain answer"}]}}"#;
        assert_eq!(
            super::ADAPTER.final_response(text, path).as_deref(),
            Some("plain answer")
        );
        let tool = r#"{"kind":"ActionEvent","source":"agent","tool_name":"terminal","action":{"message":"tool data"}}"#;
        let launcher = r#"{"type":"result","text":"native answer"}"#;
        assert_eq!(
            super::ADAPTER.final_response(launcher, path).as_deref(),
            Some("native answer")
        );
        let user = r#"{"kind":"MessageEvent","source":"user","llm_message":{"role":"user","content":[{"type":"text","text":"prompt"}]}}"#;
        assert!(super::ADAPTER.final_response(user, path).is_none());
        assert!(
            super::ADAPTER
                .final_response(&format!("{event}\n{user}"), path)
                .is_none()
        );
        let feedback = r#"{"kind":"MessageEvent","source":"environment","llm_message":{"role":"user","content":[{"type":"text","text":"retry"}]}}"#;
        assert!(
            super::ADAPTER
                .final_response(&format!("{event}\n{feedback}"), path)
                .is_none()
        );
        let no_result = r#"{"type":"result","text":null}"#;
        assert!(
            super::ADAPTER
                .final_response(&format!("{event}\n{no_result}"), path)
                .is_none()
        );
        assert_eq!(
            super::ADAPTER
                .final_response(&format!("{event}\n{launcher}"), path)
                .as_deref(),
            Some("native answer")
        );
        let agent_error = r#"{"kind":"AgentErrorEvent","source":"agent","error":"finish failed","tool_name":"finish","action_id":"a"}"#;
        assert!(
            super::ADAPTER
                .final_response(&format!("{event}\n{agent_error}"), path)
                .is_none()
        );
        let rejected = r#"{"kind":"UserRejectObservation","source":"environment","tool_name":"finish","action_id":"a"}"#;
        assert!(
            super::ADAPTER
                .final_response(&format!("{event}\n{rejected}"), path)
                .is_none()
        );
        assert!(
            super::ADAPTER
                .final_response(&format!("{text}\n{tool}"), path)
                .is_none()
        );
        assert!(
            super::ADAPTER
                .final_response(&format!("{event}\n{{"), path)
                .is_none()
        );
    }

    #[test]
    fn native_launcher_only_reports_completed_final_answers() {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/openhands_native_final.py");
        let output = std::process::Command::new("python3")
            .arg(script)
            .output()
            .expect("run offline native launcher boundary tests");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn tool_event_content_is_not_usage_when_state_is_missing() {
        let output = r#"{"kind":"ObservationEvent","content":"{\"usage\":{\"input_tokens\":999,\"output_tokens\":888}}"}"#;
        let output_usage = crate::usage::from_sources("openrouter", "openhands", output, "", None);
        let usage = crate::usage::with_artifact_usage(output_usage, None);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (None, None, None)
        );
    }
    use super::*;
    use crate::agents::AgentAdapter;
    use std::process::Command;

    fn connection() -> crate::endpoint::Connection {
        crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:4242/v1".into(),
            credential: Some("fixture-key".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        }
    }

    fn environment(command: &Command) -> std::collections::HashMap<String, String> {
        command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn native_record_selects_sdk_launcher_and_publishes_its_frozen_sidecar() {
        let root =
            std::env::temp_dir().join(format!("asb-openhands-native-{}", std::process::id()));
        let run = root.join("run");
        let tape = root.join("provider.jsonl");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&tape, b"{}\n").unwrap();
        let config = crate::benchmark::BenchmarkConfig {
            planning: None,
            agent: "openhands".into(),
            provider: "cli2key".into(),
            model: "fixture".into(),
            workload: "adapter-smoke".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "benchmark.log".into(),
            measurements: None,
            retention: Default::default(),
            mcp_servers: Vec::new(),
        };
        let specification = command(&CommandContext {
            executable: "openhands",
            config: &config,
            endpoint: Some("http://127.0.0.1:4242/v1"),
            interface: Some(crate::endpoint::Interface::OpenAiChat),
            prompt: "solve the fixture",
            max_steps: None,
            native_tool_mode: Some(crate::tape::Mode::Record),
            native_tool: None,
        });
        assert_eq!(specification.program, "openhands");
        assert_eq!(specification.args.first().map(String::as_str), Some("-c"));
        assert!(
            specification
                .args
                .windows(2)
                .any(|pair| pair == ["--mode", "record"])
        );

        let mut owner = super::super::ArtifactOwner::prepare(&run, ARTIFACTS).unwrap();
        let mut command = Command::new("openhands");
        ADAPTER
            .prepare(&mut PreparationContext {
                command: &mut command,
                model: "fixture",
                connection: &connection(),
                workspace: &root,
                runtime_dir: &root,
                run_dir: &run,
                mcp_servers: &[],
                mcp_tape: None,
                tape: Some(super::super::TapeContext {
                    mode: crate::tape::Mode::Record,
                    cassette: &tape,
                }),
            })
            .unwrap();
        let sidecar = run.join("openhands/native-tool-sidecar.jsonl");
        assert_eq!(
            environment(&command).get("ASB_OPENHANDS_NATIVE_RECORD_SIDECAR"),
            Some(&sidecar.display().to_string())
        );
        // ArtifactOwner, not adapter preparation, owns this pre-claimed
        // namespace. The SDK bridge may now create the single declared file.
        assert!(run.join("openhands").is_dir());
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&sidecar, b"{\"schema_version\":1}\n").unwrap();
        let frozen = owner.freeze();
        assert!(!frozen.error);
        let artifacts = collect(&CollectionContext {
            frozen: &frozen,
            provider_capture: None,
            run_dir: &run,
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
        });
        assert_eq!(
            artifacts.native_tool_sidecar.as_deref(),
            Some(b"{\"schema_version\":1}\n".as_slice())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sdk_launcher_uses_the_terminal_factory_and_private_bridge_only() {
        assert!(SDK_LAUNCHER.contains("def with_terminal_executor(agent: Any, executor: Any)"));
        assert!(SDK_LAUNCHER.contains("class NativeTerminalExecutor(ToolExecutor):"));
        assert!(SDK_LAUNCHER.contains("class UnrecordedTerminalExecutor(ToolExecutor):"));
        assert!(SDK_LAUNCHER.contains("from openhands.tools.terminal import TerminalTool"));
        assert!(SDK_LAUNCHER.contains("tool.name != TerminalTool.name"));
        assert!(SDK_LAUNCHER.contains("Tool(name=TerminalTool.name, params=params)"));
        assert!(SDK_LAUNCHER.contains("params[\"executor\"] = executor"));
        assert!(SDK_LAUNCHER.contains("agent.model_copy(update={\"tools\": tools})"));
        assert!(!SDK_LAUNCHER.contains("register_tool("));
        assert!(!SDK_LAUNCHER.contains("setattr("));
        assert!(SDK_LAUNCHER.contains("def main()"));
        assert!(SDK_LAUNCHER.contains("__asb-agent-bridge\", \"openhands"));
        assert!(!SDK_LAUNCHER.contains("native-tool-sidecar.jsonl"));
        // The SDK persists its supplied agent during construction. The bridge
        // owns process handles, so it must be attached only afterwards to the
        // in-memory agent for this one adapter invocation.
        assert!(SDK_LAUNCHER.contains("base_agent = get_default_cli_agent(llm)"));
        assert!(
            SDK_LAUNCHER
                .contains("persistence_dir=os.environ.get(\"OPENHANDS_CONVERSATIONS_DIR\")")
        );
        assert!(
            SDK_LAUNCHER
                .contains("conversation.agent = with_terminal_executor(base_agent, executor)")
        );
        assert!(SDK_LAUNCHER.contains("OpenHands replay requested an unrecorded terminal action"));
        assert!(SDK_LAUNCHER.contains("conversation.send_message(options.task)"));
        assert!(SDK_LAUNCHER.contains("conversation.run()"));
        assert!(!SDK_LAUNCHER.contains("conversation.ask_agent("));
    }

    #[test]
    fn sdk_launcher_injects_the_installed_cli_terminal_tool() {
        let Some(python) = std::env::var_os("ASB_TEST_OPENHANDS_PYTHON") else {
            // OpenHands is intentionally a project-local optional tool, not a
            // test dependency. The installed-SDK regression is opt-in.
            return;
        };
        let root =
            std::env::temp_dir().join(format!("asb-openhands-sdk-launcher-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let launcher = root.join("openhands_sdk_launcher.py");
        std::fs::write(&launcher, SDK_LAUNCHER).unwrap();
        let script = r#"
import importlib.util
import sys

from openhands.tools.terminal import TerminalTool
from openhands_cli.utils import get_default_cli_tools

spec = importlib.util.spec_from_file_location("asb_openhands_launcher", sys.argv[1])
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class Agent:
    def __init__(self, tools):
        self.tools = tools
    def model_copy(self, *, update):
        return Agent(update["tools"])

original = Agent(get_default_cli_tools())
executor = object()
configured = module.with_terminal_executor(original, executor)
terminal = [tool for tool in configured.tools if tool.name == TerminalTool.name]
assert len(terminal) == 1
assert terminal[0].params["executor"] is executor
assert "executor" not in [tool for tool in original.tools if tool.name == TerminalTool.name][0].params
try:
    module.UnrecordedTerminalExecutor()(None)
    raise AssertionError("unrecorded terminal action was accepted")
except RuntimeError as error:
    assert "unrecorded terminal action" in str(error)
"#;
        let status = std::process::Command::new(python)
            .arg("-c")
            .arg(script)
            .arg(&launcher)
            .env("OPENHANDS_SUPPRESS_BANNER", "1")
            .status()
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
        assert!(status.success());
    }

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
        assert_eq!(conversation_steps(&doc), Some(3));
    }

    #[test]
    fn incomplete_metrics_and_empty_initial_stats_are_unknown() {
        let doc = serde_json::json!({"stats":{"usage_to_metrics":{"agent":{"response_latencies":[{},{}],"token_usages":[{"prompt_tokens":10,"completion_tokens":3}]}}}});
        assert_eq!(conversation_usage(&doc, false).total_tokens, None);
        assert_eq!(conversation_steps(&doc), None);
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
            provider_capture: None,
            run_dir: &root,
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
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
