use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, PreparationContext,
    StaticAdapter,
};
use crate::{Error, install::Recipe};

const ARTIFACTS: &[super::super::ArtifactSpec] = &[super::super::ArtifactSpec {
    namespace: "openjiuwen",
    filename: "native-tool-sidecar.jsonl",
    format: super::super::ArtifactFormat::JsonLines,
}];

// OpenJiuwen recreates a provider-wire tool-result id after replaying an
// Anthropic tool-use response. The response's immediate assistant turn is
// already materialized by llmtape, so use that exact ordered turn as the
// adapter-local authority for the following result turn. This is deliberately
// narrower than a generic id normalizer: malformed, missing, reordered, or
// non-adjacent tool exchanges fail before a request is forwarded.
const TAPE_WIRE_EXTENSION: &str = r#"
function fail(reason) {
  throw new Error(`OpenJiuwen tool-result replay relation is invalid: ${reason}`);
}

function object(value) {
  return value && typeof value === "object" && !Array.isArray(value);
}

export function normalizeRequest(request) {
  const value = request?.body;
  if (!request?.url?.includes("/messages") || !object(value) || !Array.isArray(value.messages)) {
    return value;
  }
  for (let index = 0; index < value.messages.length; index += 1) {
    const resultTurn = value.messages[index];
    if (!object(resultTurn) || resultTurn.role !== "user" || !Array.isArray(resultTurn.content)) continue;
    const results = resultTurn.content.filter((block) => object(block) && block.type === "tool_result");
    if (results.length === 0) continue;
    if (results.length !== resultTurn.content.length || index === 0) {
      fail("tool results must occupy one non-initial user turn");
    }
    const callTurn = value.messages[index - 1];
    if (!object(callTurn) || callTurn.role !== "assistant" || !Array.isArray(callTurn.content)) {
      fail("tool results require the immediately preceding assistant tool-use turn");
    }
    const calls = callTurn.content.filter((block) => object(block) && block.type === "tool_use");
    if (calls.length !== results.length) {
      fail("tool-use and tool-result counts must match exactly");
    }
    const callIds = new Set();
    const resultIds = new Set();
    for (let position = 0; position < results.length; position += 1) {
      const call = calls[position];
      const result = results[position];
      if (typeof call.id !== "string" || call.id.length === 0
          || typeof result.tool_use_id !== "string" || result.tool_use_id.length === 0) {
        fail("tool-use and tool-result ids must be non-empty strings");
      }
      if (callIds.has(call.id) || resultIds.has(result.tool_use_id)) {
        fail("tool-use and tool-result ids must be unique within one pair");
      }
      callIds.add(call.id);
      resultIds.add(result.tool_use_id);
      // OpenJiuwen reparses the same historical assistant call with a new
      // uuid on later turns. Canonicalize both sides of this one proven pair
      // together, before llmtape's ordinary structural comparison. The
      // Anthropic protocol requires only that the paired ids agree, so this
      // remains valid when forwarding a live record request as well.
      const marker = `<asb-openjiuwen-tool-pair-${index}-${position}>`;
      call.id = marker;
      result.tool_use_id = marker;
    }
  }
  return value;
}
"#;

pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "openjiuwen",
    "openjiuwen",
    Recipe::Python {
        package: "openjiuwen[cli]==0.1.19.post2",
        // filelock 4.2.2 stalls while the pinned OpenJiuwen client acquires
        // its filesystem locks. 3.32.7 satisfies its >=3.25 requirement and
        // is verified by the opt-in real-client fixture test.
        dependencies: &["filelock==3.32.7"],
        executable: "openjiuwen",
    },
    &[crate::endpoint::MESSAGES_KEY],
    command,
    prepare_adapter,
)
.with_estimated_disk_mib(768)
.with_replay_credential_vars(&["OPENJIUWEN_API_KEY"])
.with_output_usage(output_usage)
.with_step_count(step_count)
.with_final_response(final_response)
.with_final_response_failure(final_response_failure)
.with_native_tool_transport()
.with_artifact_specs(ARTIFACTS)
.with_tape_wire_extension(TAPE_WIRE_EXTENSION)
.with_collection(collect)
.with_process(process);

fn final_response_failure(response: Option<&str>) -> Option<String> {
    response.is_none().then(|| "OpenJiuwen exited without a final response; the last event appears to be an unexecuted tool call. Retry with a compatible OpenJiuwen client or inspect the benchmark log.".into())
}

/// Launch the installed Python entry point with a narrow, in-process fix for
/// upstream's loss of initial usage when a streaming delta carries a partial
/// usage object. Non-Python custom executables continue to run unmodified.
fn process(executable: &str) -> Result<crate::launch::ProcessLaunch, Error> {
    use std::io::{BufRead, BufReader, Read};
    use std::path::{Path, PathBuf};
    let path = Path::new(executable);
    let path = if path.components().count() > 1 {
        Some(path.to_path_buf())
    } else {
        std::env::var_os("PATH").and_then(|value| {
            std::env::split_paths(&value)
                .map(|dir| dir.join(executable))
                .find(|candidate| candidate.is_file())
        })
    };
    let Some(path) = path else {
        return Ok(crate::launch::ProcessLaunch::new(executable));
    };
    let first = std::fs::File::open(&path)
        .ok()
        .and_then(|file| BufReader::new(file.take(4096)).lines().next()?.ok());
    let interpreter = first
        .as_deref()
        .and_then(|line| line.strip_prefix("#!"))
        .map(str::trim)
        .filter(|line| {
            !line.contains(' ')
                && Path::new(line)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| name.starts_with("python"))
        })
        .map(PathBuf::from);
    let Some(interpreter) = interpreter else {
        return Ok(crate::launch::ProcessLaunch::new(executable));
    };
    let mut command = crate::launch::ProcessLaunch::new(interpreter);
    command
        .arg("-c")
        .arg(include_str!("openjiuwen/usage_compat.py"))
        .arg(path);
    Ok(command)
}
fn command(context: &CommandContext<'_>) -> CommandSpec {
    let mut args = vec![
        "-v".into(),
        "-w".into(),
        context.config.workspace.clone(),
        "--model".into(),
        context.config.model.clone(),
        "--provider".into(),
        "anthropic".into(),
        "--api-base".into(),
        context.endpoint.unwrap_or_default().into(),
    ];
    if let Some(native) = context.native_tool {
        args.extend([
            "--asb-native-tool-replay".into(),
            native.path.to_string_lossy().into_owned(),
        ]);
    }
    args.extend(["run".into(), "-f".into(), "stream-json".into(), format!(
        "{}\n\nDo not use todo/planning tools for this run. Execute the requested work directly and return a final response when complete.", context.prompt
    )]);
    CommandSpec {
        program: "openjiuwen".into(),
        args,
        prompt_delivery: super::PromptDelivery::Embedded,
    }
}

fn output_usage(
    stdout: &str,
    _stderr: &str,
    input_includes_cache: bool,
) -> crate::measurements::TokenUsage {
    let mut sum = crate::measurements::TokenUsage::default();
    let mut seen = false;
    let mut cache_complete = true;
    for event in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if event.get("type").and_then(|v| v.as_str()) != Some("llm_usage") {
            continue;
        }
        let Some(metadata) = event.pointer("/payload/usage_metadata") else {
            return Default::default();
        };
        let mut usage = super::super::usage_fields(metadata);
        // The SDK normally adds cache reads and writes to the wire input.
        // Some compatible providers already include them in the wire input.
        if input_includes_cache {
            let extra = usage.cached_tokens.unwrap_or(0).saturating_add(
                metadata
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            );
            usage.input_tokens = usage
                .input_tokens
                .and_then(|input| input.checked_sub(extra));
            usage.total_tokens = None;
        }
        if usage.input_tokens.is_none()
            || usage.output_tokens.is_none()
            || usage.input_tokens == Some(0)
        {
            return Default::default();
        }
        cache_complete &= usage.cached_tokens.is_some();
        sum = sum.add(usage);
        seen = true;
    }
    if seen {
        if !cache_complete {
            sum.cached_tokens = None;
        }
        sum.with_derived_total()
    } else {
        Default::default()
    }
}

/// OpenJiuwen emits one `llm_usage` record after every completed model call.
/// Do not count output or answer records: they can be streamed fragments of
/// the same model step.
fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    let steps = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| {
            event.get("type").and_then(|value| value.as_str()) == Some("llm_usage")
                && event.pointer("/payload/usage_metadata").is_some()
        })
        .count()
        .try_into()
        .ok()?;
    (steps > 0).then_some(steps)
}

fn final_response(output: &str, _workspace: &std::path::Path) -> Option<String> {
    let mut text = String::new();
    let mut answer = String::new();
    let mut finalized = None;
    for event in output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if event.get("type").and_then(|v| v.as_str()) == Some("answer")
            && event
                .pointer("/payload/result_type")
                .and_then(|v| v.as_str())
                == Some("answer")
        {
            finalized = event
                .pointer("/payload/output")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
        }
        let target = match event.get("type").and_then(|v| v.as_str()) {
            Some("llm_output") => &mut text,
            Some("answer") => &mut answer,
            _ => continue,
        };
        if let Some(payload) = event.get("payload")
            && let Some(content) = payload
                .as_str()
                .or_else(|| payload.get("content").and_then(|v| v.as_str()))
                .or_else(|| payload.get("output").and_then(|v| v.as_str()))
        {
            target.push_str(content);
        }
    }
    // A typed final answer supersedes llm_output chunks, which can contain
    // earlier turns. Preserve legacy chunk-only captures as a fallback.
    if let Some(finalized) = finalized {
        (!finalized.trim().is_empty()).then_some(finalized)
    } else if !text.trim().is_empty() {
        Some(text)
    } else if !answer.trim().is_empty() {
        Some(answer)
    } else {
        None
    }
}

fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    context.command.env(
        "OPENJIUWEN_API_KEY",
        connection.credential.as_deref().unwrap_or("asb-local"),
    );
    context.command.env(
        "ASB_OPENJIUWEN_NATIVE_TOOL_RECORD",
        context.run_dir.join("openjiuwen/native-tool-sidecar.jsonl"),
    );
    let workspace = std::fs::canonicalize(context.workspace)
        .unwrap_or_else(|_| context.workspace.to_path_buf());
    context
        .command
        .env("ASB_OPENJIUWEN_NATIVE_TOOL_WORKSPACE", workspace);
    Ok(())
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    AgentArtifacts {
        native_tool_sidecar: context
            .frozen
            .bytes("native-tool-sidecar.jsonl")
            .map(ToOwned::to_owned)
            .filter(|bytes| !bytes.is_empty()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn terminal_answer_supersedes_intermediate_llm_output() {
        let output = concat!(
            "{\"type\":\"llm_output\",\"payload\":{\"content\":\"intermediate\"}}\n",
            "{\"type\":\"answer\",\"payload\":{\"output\":\"final answer\",\"result_type\":\"answer\"}}\n"
        );
        assert_eq!(
            super::final_response(output, std::path::Path::new(".")).as_deref(),
            Some("final answer")
        );
    }

    use super::{command, step_count};
    use crate::agents::{ArtifactOwner, CollectionContext, CommandContext};
    use crate::benchmark::BenchmarkConfig;

    #[test]
    fn counts_usage_records_as_completed_model_steps() {
        let output = concat!(
            "{\"type\":\"llm_output\",\"payload\":\"fragment\"}\n",
            "{\"type\":\"llm_usage\",\"payload\":{\"usage_metadata\":{}}}\n",
            "{\"type\":\"answer\",\"payload\":\"done\"}\n",
            "{\"type\":\"llm_usage\",\"payload\":{\"usage_metadata\":{}}}\n"
        );
        assert_eq!(step_count(output, ""), Some(2));
        assert_eq!(step_count("{\"type\":\"answer\"}\n", ""), None);
    }

    #[test]
    fn tape_wire_extension_rewrites_only_a_complete_adjacent_tool_pair() {
        let root = std::env::temp_dir().join(format!(
            "asb-openjiuwen-wire-extension-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("extension.mjs"), super::TAPE_WIRE_EXTENSION).unwrap();
        std::fs::write(
            root.join("test.mjs"),
            r#"import { normalizeRequest } from "./extension.mjs";
const valid = {
  url: "/v1/messages",
  body: { messages: [
    { role: "assistant", content: [{ type: "tool_use", id: "recorded", name: "read", input: {} }] },
    { role: "user", content: [{ type: "tool_result", tool_use_id: "regenerated", content: [{ type: "text", text: "ok" }] }] },
  ] },
};
if (normalizeRequest(valid).messages[0].content[0].id !== "<asb-openjiuwen-tool-pair-1-0>"
    || valid.body.messages[1].content[0].tool_use_id !== "<asb-openjiuwen-tool-pair-1-0>") throw new Error("valid rewrite failed");
const invalid = [
  { url: "/v1/messages", body: { messages: [{ role: "user", content: [{ type: "tool_result", tool_use_id: "x", content: [] }] }] } },
  { url: "/v1/messages", body: { messages: [
    { role: "assistant", content: [{ type: "tool_use", id: "x", name: "read", input: {} }, { type: "tool_use", id: "y", name: "write", input: {} }] },
    { role: "user", content: [{ type: "tool_result", tool_use_id: "x", content: [] }] },
  ] } },
  { url: "/v1/messages", body: { messages: [
    { role: "assistant", content: [{ type: "text", text: "reordered" }] },
    { role: "user", content: [{ type: "tool_result", tool_use_id: "x", content: [] }] },
  ] } },
  { url: "/v1/messages", body: { messages: [
    { role: "assistant", content: [{ type: "tool_use", id: "", name: "read", input: {} }] },
    { role: "user", content: [{ type: "tool_result", tool_use_id: "x", content: [] }] },
  ] } },
  { url: "/v1/messages", body: { messages: [
    { role: "assistant", content: [{ type: "tool_use", id: "x", name: "read", input: {} }, { type: "tool_use", id: "x", name: "write", input: {} }] },
    { role: "user", content: [{ type: "tool_result", tool_use_id: "x", content: [] }, { type: "tool_result", tool_use_id: "y", content: [] }] },
  ] } },
];
for (const request of invalid) {
  let rejected = false;
  try { normalizeRequest(request); } catch { rejected = true; }
  if (!rejected) throw new Error("malformed tool pair was accepted");
}
"#,
        )
        .unwrap();
        let output = std::process::Command::new("node")
            .arg(root.join("test.mjs"))
            .arg("replay")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_tool_recording_is_collected_from_the_frozen_guest_artifact() {
        let root =
            std::env::temp_dir().join(format!("asb-openjiuwen-artifact-{}", std::process::id()));
        let run = root.join("run");
        let mut owner = ArtifactOwner::prepare(&run, super::ARTIFACTS).unwrap();
        let sidecar = run.join("openjiuwen/native-tool-sidecar.jsonl");
        std::fs::write(&sidecar, b"{\"event\":\"launch\"}\n").unwrap();
        let frozen = owner.freeze();
        assert!(!frozen.error);
        let artifacts = super::collect(&CollectionContext {
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
            Some(b"{\"event\":\"launch\"}\n".as_slice())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_terminal_answer_is_an_adapter_policy_for_native_and_guest_results() {
        use crate::agents::AgentAdapter;
        assert!(
            super::ADAPTER
                .final_response_failure(None)
                .unwrap()
                .contains("exited without a final response")
        );
        assert!(
            super::ADAPTER
                .final_response_failure(Some("completed"))
                .is_none()
        );
        assert!(
            crate::agents::find("codex")
                .unwrap()
                .final_response_failure(None)
                .is_none()
        );
    }

    #[test]
    fn installer_pins_a_tested_compatible_filelock() {
        let crate::install::Recipe::Python { dependencies, .. } = super::ADAPTER.recipe else {
            panic!("OpenJiuwen must use a Python recipe");
        };
        assert!(dependencies.contains(&"filelock==3.32.7"));
    }

    #[test]
    fn enables_verbose_mode_for_deterministic_noninteractive_runs() {
        let config = BenchmarkConfig {
            planning: None,
            agent: "openjiuwen".into(),
            workload: "bug-fix".into(),
            provider: "cli2key".into(),
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
            executable: "openjiuwen",
            config: &config,
            endpoint: Some("http://127.0.0.1:3000/v1"),
            interface: Some(crate::endpoint::Interface::AnthropicMessages),
            prompt: "repair",
            max_steps: None,
            native_tool_mode: None,
            native_tool: None,
        });
        assert_eq!(spec.args.first().map(String::as_str), Some("-v"));
        assert!(
            spec.args
                .windows(2)
                .any(|pair| pair[0] == "-w" && pair[1] == "workspace")
        );
        assert!(
            spec.args
                .windows(2)
                .any(|pair| pair[0] == "-f" && pair[1] == "stream-json")
        );
    }

    #[test]
    fn stream_usage_sums_real_calls_only_and_reassembles_output() {
        let output = concat!(
            "{\"type\":\"context_usage\",\"payload\":{\"input_tokens\":999999}}\n",
            "{\"type\":\"llm_output\",\"payload\":{\"content\":\"src/cli.py \"}}\n",
            "{\"type\":\"llm_usage\",\"payload\":{\"usage_metadata\":{\"input_tokens\":9326,\"output_tokens\":58,\"cache_read_tokens\":12}}}\n",
            "{\"type\":\"llm_output\",\"payload\":{\"content\":\"src/registry.py lookup\"}}\n",
            "{\"type\":\"llm_usage\",\"payload\":{\"usage_metadata\":{\"input_tokens\":9401,\"output_tokens\":66,\"cache_read_tokens\":34}}}\n",
            "{\"type\":\"answer\",\"payload\":{\"content\":\"duplicate answer\"}}\n",
        );
        let usage = crate::usage::from_sources("anthropic", "openjiuwen", output, "", None);
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cached_tokens,
                usage.total_tokens
            ),
            (Some(18727), Some(124), Some(46), Some(18851))
        );
        assert_eq!(
            super::final_response(output, std::path::Path::new(".")),
            Some("src/cli.py src/registry.py lookup".into())
        );
        let unknown = crate::usage::from_sources(
            "cli2key",
            "openjiuwen",
            "{\"type\":\"llm_usage\",\"payload\":{\"usage_metadata\":{\"input_tokens\":0,\"output_tokens\":0}}}\n",
            "",
            None,
        );
        assert_eq!(unknown.total_tokens, None);
    }

    #[test]
    fn inclusive_wire_cache_is_not_counted_twice() {
        let output = r#"{"type":"llm_usage","payload":{"usage_metadata":{"input_tokens":1000,"output_tokens":10,"cache_read_tokens":200,"cache_creation_input_tokens":50}}}"#;
        let inclusive = super::output_usage(output, "", true);
        assert_eq!(
            (
                inclusive.input_tokens,
                inclusive.output_tokens,
                inclusive.total_tokens
            ),
            (Some(750), Some(10), Some(760))
        );
        let standard = super::output_usage(output, "", false);
        assert_eq!(
            (standard.input_tokens, standard.total_tokens),
            (Some(1000), Some(1010))
        );
    }

    #[cfg(unix)]
    #[test]
    fn compatibility_launcher_preserves_split_stream_usage_without_changing_content() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("asb-openjiuwen-usage-{}", std::process::id()));
        let module = root.join("openjiuwen/core/foundation/llm/schema");
        std::fs::create_dir_all(&module).unwrap();
        std::fs::write(module.join("message_chunk.py"), r#"
class Usage:
    def __init__(self, **fields): self.__dict__.update(fields)
    def model_copy(self, update): return Usage(**(self.__dict__ | update))
class AssistantMessageChunk:
    def __init__(self, content, usage_metadata): self.content, self.usage_metadata = content, usage_metadata
    def __add__(self, other): return AssistantMessageChunk(self.content + other.content, other.usage_metadata or self.usage_metadata)
"#).unwrap();
        let python = std::process::Command::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            .unwrap();
        assert!(python.status.success());
        let python = String::from_utf8(python.stdout).unwrap();
        let entry = root.join("openjiuwen-test");
        std::fs::write(&entry, format!(r#"#!{}
from openjiuwen.core.foundation.llm.schema.message_chunk import AssistantMessageChunk as C, Usage
first = C('left', Usage(input_tokens=120, output_tokens=0, cache_read_tokens=7))
second = C('right', Usage(input_tokens=0, output_tokens=9, cache_read_tokens=0))
value = first + second
print(value.content, value.usage_metadata.input_tokens, value.usage_metadata.output_tokens, value.usage_metadata.cache_read_tokens, value.usage_metadata.total_tokens)
"#, python.trim())).unwrap();
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = super::process(entry.to_str().unwrap())
            .unwrap()
            .direct()
            .env("PYTHONPATH", &root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "leftright 120 9 7 129"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn native_tool_replay_never_executes_any_built_in_direct_tool() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "asb-openjiuwen-native-replay-{}",
            std::process::id()
        ));
        let package = root.join("openjiuwen");
        for relative in [
            "core/foundation/llm/schema",
            "core/foundation/tool",
            "core/context_engine/context",
            "core/single_agent",
            "core/single_agent/agents",
            "harness/cli/agent",
            "harness/tools/shell",
            "harness/tools",
        ] {
            std::fs::create_dir_all(package.join(relative)).unwrap();
        }
        for relative in [
            "__init__.py",
            "core/__init__.py",
            "core/foundation/__init__.py",
            "core/foundation/llm/__init__.py",
            "core/foundation/llm/schema/__init__.py",
            "core/foundation/tool/__init__.py",
            "core/context_engine/__init__.py",
            "core/context_engine/context/__init__.py",
            "core/single_agent/__init__.py",
            "core/single_agent/agents/__init__.py",
            "harness/__init__.py",
            "harness/cli/__init__.py",
            "harness/cli/agent/__init__.py",
            "harness/tools/__init__.py",
            "harness/tools/shell/__init__.py",
        ] {
            std::fs::write(package.join(relative), "").unwrap();
        }
        // The adapter-private replay selector must be removed before an
        // upstream package import can inspect argv and alter its provider
        // feature selection.
        std::fs::write(
            package.join("__init__.py"),
            "import sys\nassert '--asb-native-tool-replay' not in sys.argv\n",
        )
        .unwrap();
        std::fs::write(
            package.join("harness/cli/agent/factory.py"),
            r#"
class Config:
    enable_read_image_multimodal = None
    parallel_tool_calls = None
class Agent:
    _deep_config = Config()
def create_agent(_cfg):
    return Agent(), object()
"#,
        )
        .unwrap();
        std::fs::write(
            package.join("core/foundation/llm/schema/message_chunk.py"),
            r#"
class AssistantMessageChunk:
    usage_metadata = None
    def __add__(self, other): return other
"#,
        )
        .unwrap();
        std::fs::write(
            package.join("core/foundation/tool/schema.py"),
            r#"
class ToolOutput:
    def __init__(self, success=True, data=None, error=None):
        self.success, self.data, self.error = success, data or {}, error
    def model_dump(self):
        return {"success": self.success, "data": self.data, "error": self.error}
    @classmethod
    def model_validate(cls, value):
        return cls(value["success"], value.get("data"), value.get("error"))
"#,
        )
        .unwrap();
        std::fs::write(
            package.join("core/foundation/tool/base.py"),
            r#"
class Tool:
    def render_for_llm(self, output):
        return "base:" + str(output.model_dump())
"#,
        )
        .unwrap();
        std::fs::write(
            package.join("core/context_engine/context/context.py"),
            r#"
class Context:
    def __init__(self): self.messages = []
    async def add_messages(self, message): self.messages.append(message)
"#,
        )
        .unwrap();
        std::fs::write(
            package.join("core/single_agent/ability_manager.py"),
            r#"
import os
class AbilityManager:
    @staticmethod
    def _render_tool_result(tool, _tool_call, result):
        return tool.render_for_llm(result)
    async def execute(self, result=None, message=None):
        class SyntheticResult:
            def model_dump(self):
                return {"success": False, "data": {"unrelated": True}, "error": None}
        pairs = [
            "non-tool",
            (SyntheticResult(), ToolMessage("synthetic-before", "unrelated-before")),
            (result, message),
            (SyntheticResult(), ToolMessage("synthetic-after", "unrelated-after")),
        ]
        if os.environ.get("ASB_TEST_EXTRA_NATIVE_MESSAGE"):
            pairs.append((result, ToolMessage(message.tool_call_id, "extra-native")))
        return pairs
class ToolMessage:
    def __init__(self, tool_call_id, content):
        self.tool_call_id, self.content = tool_call_id, content
"#,
        )
        .unwrap();
        std::fs::write(
            package.join("core/single_agent/agents/react_agent.py"),
            r#"
import asyncio
from openjiuwen.core.single_agent.ability_manager import AbilityManager
class ReActAgent:
    async def _execute_tool_call(self, _a, _b, _c, context, result, message):
        # Keep this invocation active while its sibling enters the same
        # context hook.  The adapter must not restore the original method
        # until both concurrent calls finish.
        if message.tool_call_id == "concurrent-second":
            await asyncio.sleep(0.02)
        results = await AbilityManager().execute(result, message)
        for entry in results:
            if isinstance(entry, tuple) and len(entry) == 2:
                await context.add_messages(entry[1])
        return results
"#,
        )
        .unwrap();
        let tools = r#"
import asyncio
import os
import json
from openjiuwen.core.foundation.tool.schema import ToolOutput
from openjiuwen.core.foundation.tool.base import Tool
class _Tool(Tool):
    label = ""
    def __init__(self):
        self._renders = 0
    async def invoke(self, inputs, **kwargs):
        # Grep delegates to Glob in the real client.  The adapter sidecar
        # must preserve this nested direct-tool relation even though replay
        # returns Grep's captured outer result without running either tool.
        if self.label == "grep" and inputs.get("nested"):
            await GlobTool().invoke({"nested": inputs["nested"]})
        # Two independent roots complete in the opposite order in which they
        # were dispatched. Their task-local ancestry must remain root-level,
        # and replay must use dispatch ordinal rather than sidecar append
        # order.
        if inputs.get("concurrent") == "first":
            await asyncio.sleep(0.01)
        elif inputs.get("concurrent") == "second":
            await asyncio.sleep(0)
        with open(os.environ["ASB_TEST_NATIVE_EXECUTED"], "a", encoding="utf-8") as marker:
            marker.write(self.label + "\n")
        return ToolOutput(data={"tool": self.label, "inputs": inputs})
    def render_for_llm(self, output):
        self._renders += 1
        return os.environ["ASB_TEST_RENDER_PREFIX"] + str(self._renders) + ":" + json.dumps(output.model_dump(), sort_keys=True)
class ReadFileTool(_Tool): label = "read_file"
class WriteFileTool(_Tool): label = "write_file"
class EditFileTool(_Tool): label = "edit_file"
class GlobTool(_Tool): label = "glob"
class ListDirTool(_Tool): label = "list_dir"
class GrepTool(_Tool): label = "grep"
"#;
        std::fs::write(package.join("harness/tools/filesystem.py"), tools).unwrap();
        std::fs::write(
            package.join("harness/tools/shell/__init__.py"),
            format!("{}\nclass BashTool(_Tool): label = 'bash'\n", tools),
        )
        .unwrap();
        std::fs::write(
            package.join("harness/tools/code.py"),
            format!("{}\nclass CodeTool(_Tool): label = 'code'\n", tools),
        )
        .unwrap();
        let python = std::process::Command::new("python3")
            .args(["-c", "import sys; print(sys.executable)"])
            .output()
            .unwrap();
        assert!(python.status.success());
        let python = String::from_utf8(python.stdout).unwrap();
        let entry = root.join("openjiuwen-native-test");
        std::fs::write(
            &entry,
            format!(
                r#"#!{}
import asyncio, json
import os
from openjiuwen.harness.cli.agent import factory
from openjiuwen.harness.tools.filesystem import ReadFileTool, WriteFileTool, EditFileTool, GlobTool, ListDirTool, GrepTool
from openjiuwen.harness.tools.shell import BashTool
from openjiuwen.harness.tools.code import CodeTool
from openjiuwen.core.single_agent.ability_manager import AbilityManager, ToolMessage
from openjiuwen.core.single_agent.agents.react_agent import ReActAgent
from openjiuwen.core.context_engine.context.context import Context
async def main():
    agent, _tracker = factory.create_agent(None)
    assert agent._deep_config.enable_read_image_multimodal is False
    assert agent._deep_config.parallel_tool_calls is False
    result = []
    context = Context()
    for tool in (ReadFileTool, WriteFileTool, EditFileTool, GlobTool, ListDirTool, GrepTool, BashTool, CodeTool):
        instance = tool()
        # A wrapped native tool can call another wrapped native tool before
        # the outer result is returned to AbilityManager. The inner exchange
        # is replayed but must not consume a final ToolMessage cursor.
        inner = ReadFileTool()
        inner_output = await inner.invoke({{"inner": os.path.join(os.environ["ASB_TEST_WORKSPACE"], tool.label)}})
        # Nested direct exchange has no provider-visible final message.
        # Replay must consume it without inventing a rendering or history row.
        output = await instance.invoke({{"value": os.path.join(os.environ["ASB_TEST_WORKSPACE"], tool.label), "nested": tool.label if tool.label == "grep" else None}})
        # Final-message admission is provenance-based: a copied ToolOutput
        # has no private native marker and must not be guessed from payload.
        tool_call = type("ToolCall", (), {{"id": os.environ.get("ASB_TEST_TOOL_CALL_ID", tool.label)}})()
        first_render = AbilityManager._render_tool_result(instance, tool_call, output)
        AbilityManager._render_tool_result(instance, tool_call, output)
        message_id = os.environ.get("ASB_TEST_TOOL_CALL_ID", tool.label)
        if os.environ.get("ASB_TEST_CHANGED_FINAL_PROVENANCE"):
            message_id = "changed-" + message_id
        message = ToolMessage(message_id, "live")
        mixed = await ReActAgent()._execute_tool_call(None, None, None, context, output, message)
        assert mixed[0] == "non-tool"
        assert mixed[1][1].content == "unrelated-before"
        assert isinstance(mixed[2][1].tool_call_id, str)
        assert mixed[3][1].content == "unrelated-after"
        result.append(mixed[2][1].tool_call_id)
    async def concurrent_root(label):
        return await ReadFileTool().invoke({{"concurrent": label}})
    await asyncio.gather(concurrent_root("first"), concurrent_root("second"))
    async def concurrent_context(label):
        tool = ReadFileTool()
        output = await tool.invoke({{"context": label}})
        tool_call = type("ToolCall", (), {{"id": "concurrent-" + label}})()
        AbilityManager._render_tool_result(tool, tool_call, output)
        return await ReActAgent()._execute_tool_call(
            None, None, None, context, output, ToolMessage("concurrent-" + label, "live")
        )
    await asyncio.gather(concurrent_context("first"), concurrent_context("second"))
    print(json.dumps(result, sort_keys=True))
asyncio.run(main())
"#,
                python.trim()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sidecar = root.join("native.jsonl");
        let marker = root.join("executed");
        let record_workspace = root.join("record-workspace");
        let replay_workspace = root.join("replay-workspace");
        let record = super::process(entry.to_str().unwrap())
            .unwrap()
            .direct()
            .env("PYTHONPATH", &root)
            .env("ASB_TEST_NATIVE_EXECUTED", &marker)
            .env("ASB_TEST_WORKSPACE", &record_workspace)
            .env("ASB_TEST_RENDER_PREFIX", "record:")
            .env("ASB_OPENJIUWEN_NATIVE_TOOL_RECORD", &sidecar)
            .env("ASB_OPENJIUWEN_NATIVE_TOOL_WORKSPACE", &record_workspace)
            .output()
            .unwrap();
        assert!(
            record.status.success(),
            "{}",
            String::from_utf8_lossy(&record.stderr)
        );
        assert!(
            sidecar.exists(),
            "record stdout={} stderr={}",
            String::from_utf8_lossy(&record.stdout),
            String::from_utf8_lossy(&record.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().lines().count(),
            21
        );
        assert_eq!(
            std::fs::read_to_string(&sidecar)
                .unwrap()
                .lines()
                .filter(|line| line.contains(r#""event":"write""#))
                .count(),
            20,
            "nested helper calls must not become independent replay exchanges"
        );
        std::fs::remove_file(&marker).unwrap();
        let replay = super::process(entry.to_str().unwrap())
            .unwrap()
            .direct()
            .arg("--asb-native-tool-replay")
            .arg(&sidecar)
            .env("PYTHONPATH", &root)
            .env("ASB_TEST_NATIVE_EXECUTED", &marker)
            .env("ASB_TEST_WORKSPACE", &replay_workspace)
            .env("ASB_TEST_RENDER_PREFIX", "replay:")
            .env("ASB_OPENJIUWEN_NATIVE_TOOL_WORKSPACE", &replay_workspace)
            .output()
            .unwrap();
        assert!(
            replay.status.success(),
            "{}",
            String::from_utf8_lossy(&replay.stderr)
        );
        let record_stdout = String::from_utf8(record.stdout)
            .unwrap()
            .replace(record_workspace.to_str().unwrap(), "<asb-workspace>");
        let replay_stdout = String::from_utf8(replay.stdout)
            .unwrap()
            .replace(replay_workspace.to_str().unwrap(), "<asb-workspace>");
        assert_eq!(record_stdout, replay_stdout);
        assert!(
            !marker.exists(),
            "native tool replay executed a direct tool"
        );
        let missing_nesting = root.join("native-missing-nesting.jsonl");
        let remove_nesting = std::process::Command::new(python.trim())
            .args([
                "-c",
                r#"import base64,hashlib,json,sys
source,destination=sys.argv[1:]
events=[]
changed=False
for line in open(source, encoding="utf-8"):
    event=json.loads(line)
    if not changed and event.get("event")=="write":
        request=json.loads(base64.b64decode(event["request_b64"]))
        del request["_asb_openjiuwen_invocation"]
        raw=json.dumps(request,separators=(",",":"),sort_keys=True,ensure_ascii=False).encode()
        event["request_b64"]=base64.b64encode(raw).decode()
        event["request_sha256"]=hashlib.sha256(raw).hexdigest()
        changed=True
    events.append(event)
assert changed
with open(destination,"w",encoding="utf-8") as output:
    for event in events:
        output.write(json.dumps(event,separators=(",",":"))+"\n")
"#,
                sidecar.to_str().unwrap(),
                missing_nesting.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(remove_nesting.status.success());
        let malformed_nesting = super::process(entry.to_str().unwrap())
            .unwrap()
            .direct()
            .arg("--asb-native-tool-replay")
            .arg(&missing_nesting)
            .env("PYTHONPATH", &root)
            .env("ASB_TEST_NATIVE_EXECUTED", &marker)
            .env("ASB_TEST_WORKSPACE", &replay_workspace)
            .env("ASB_TEST_RENDER_PREFIX", "replay:")
            .env("ASB_OPENJIUWEN_NATIVE_TOOL_WORKSPACE", &replay_workspace)
            .output()
            .unwrap();
        assert!(!malformed_nesting.status.success());
        assert!(
            String::from_utf8_lossy(&malformed_nesting.stderr).contains("nesting metadata"),
            "{}",
            String::from_utf8_lossy(&malformed_nesting.stderr)
        );
        let tampered = root.join("native-tampered.jsonl");
        let tamper = std::process::Command::new(python.trim())
            .args([
                "-c",
                r#"import base64,json,sys
source,destination=sys.argv[1:]
events=[]
changed=False
for line in open(source, encoding="utf-8"):
    event=json.loads(line)
    if not changed and event.get("event")=="output":
        envelope=json.loads(base64.b64decode(event["output_b64"]))
        if envelope.get("message") is not None:
            envelope["message"]=None
            event["output_b64"]=base64.b64encode(json.dumps(envelope,separators=(",",":")).encode()).decode()
            changed=True
    events.append(event)
assert changed
with open(destination,"w",encoding="utf-8") as output:
    for event in events:
        output.write(json.dumps(event,separators=(",",":"))+"\n")
"#,
                sidecar.to_str().unwrap(),
                tampered.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(tamper.status.success());
        let mismatch = super::process(entry.to_str().unwrap())
            .unwrap()
            .direct()
            .arg("--asb-native-tool-replay")
            .arg(&tampered)
            .env("PYTHONPATH", &root)
            .env("ASB_TEST_NATIVE_EXECUTED", &marker)
            .env("ASB_TEST_WORKSPACE", &replay_workspace)
            .env("ASB_TEST_RENDER_PREFIX", "replay:")
            .env("ASB_OPENJIUWEN_NATIVE_TOOL_WORKSPACE", &replay_workspace)
            .output()
            .unwrap();
        assert!(!mismatch.status.success());
        assert!(
            String::from_utf8_lossy(&mismatch.stderr).contains("final tool message"),
            "{}",
            String::from_utf8_lossy(&mismatch.stderr)
        );
        let extra = super::process(entry.to_str().unwrap())
            .unwrap()
            .direct()
            .arg("--asb-native-tool-replay")
            .arg(&sidecar)
            .env("PYTHONPATH", &root)
            .env("ASB_TEST_NATIVE_EXECUTED", &marker)
            .env("ASB_TEST_WORKSPACE", &replay_workspace)
            .env("ASB_TEST_RENDER_PREFIX", "replay:")
            .env("ASB_TEST_EXTRA_NATIVE_MESSAGE", "1")
            .env("ASB_TEST_KEEP_RESULT_IDENTITY", "1")
            .env("ASB_OPENJIUWEN_NATIVE_TOOL_WORKSPACE", &replay_workspace)
            .output()
            .unwrap();
        assert!(!extra.status.success());
        assert!(String::from_utf8_lossy(&extra.stderr).contains("final"));
        let changed_provenance = super::process(entry.to_str().unwrap())
            .unwrap()
            .direct()
            .arg("--asb-native-tool-replay")
            .arg(&sidecar)
            .env("PYTHONPATH", &root)
            .env("ASB_TEST_NATIVE_EXECUTED", &marker)
            .env("ASB_TEST_WORKSPACE", &replay_workspace)
            .env("ASB_TEST_RENDER_PREFIX", "replay:")
            .env("ASB_TEST_CHANGED_FINAL_PROVENANCE", "1")
            .env("ASB_OPENJIUWEN_NATIVE_TOOL_WORKSPACE", &replay_workspace)
            .output()
            .unwrap();
        assert!(!changed_provenance.status.success());
        assert!(
            String::from_utf8_lossy(&changed_provenance.stderr).contains("final native message"),
            "{}",
            String::from_utf8_lossy(&changed_provenance.stderr)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
