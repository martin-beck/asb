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
.with_collection(collect);
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
    std::fs::create_dir_all(&output)
        .map_err(|_| Error::Run("cannot create OpenHands native tool artifact directory".into()))?;
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
    let mut state = None;
    let Ok(entries) = std::fs::read_dir(directory) else {
        return AgentArtifacts::default();
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return AgentArtifacts::default();
        };
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if state.is_some() {
            return AgentArtifacts::default();
        }
        state = Some(entry.path().join("base_state.json"));
    }
    let Some(value) = state.as_deref().and_then(super::super::read_usage_document) else {
        return AgentArtifacts::default();
    };
    let mut artifacts = AgentArtifacts {
        // OpenHands/LiteLLM reports cache reads alongside an inclusive input
        // total for providers such as cli2key. This accounting rule is owned
        // by the provider, not by the particular transport selected here.
        token_usage: Some(conversation_usage(
            &value,
            context.anthropic_input_includes_cache,
        )),
        steps: None,
        final_response: None,
        ..Default::default()
    };
    if let Some(sidecar) = context.frozen.bytes("native-tool-sidecar.jsonl") {
        artifacts.native_tool_sidecar = Some(sidecar.to_vec());
    }
    artifacts
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
    use super::*;

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
        assert!(SDK_LAUNCHER.contains("__asb-openhands-native-bridge"));
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
    }

    #[test]
    fn incomplete_metrics_and_empty_initial_stats_are_unknown() {
        let doc = serde_json::json!({"stats":{"usage_to_metrics":{"agent":{"response_latencies":[{},{}],"token_usages":[{"prompt_tokens":10,"completion_tokens":3}]}}}});
        assert_eq!(conversation_usage(&doc, false).total_tokens, None);
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
