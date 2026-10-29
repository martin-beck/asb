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
.with_replay_credential_vars(&["OPENJIUWEN_API_KEY"])
.with_output_usage(output_usage)
.with_final_response(final_response)
.with_final_response_failure(final_response_failure)
.with_native_tool_transport()
.with_artifact_specs(ARTIFACTS)
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

fn final_response(output: &str, _workspace: &std::path::Path) -> Option<String> {
    let mut text = String::new();
    let mut answer = String::new();
    for event in output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
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
    if !text.trim().is_empty() {
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
    use super::command;
    use crate::agents::{ArtifactOwner, CollectionContext, CommandContext};
    use crate::benchmark::BenchmarkConfig;

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
            mcp_servers: Vec::new(),
        };
        let spec = command(&CommandContext {
            executable: "openjiuwen",
            config: &config,
            endpoint: Some("http://127.0.0.1:3000/v1"),
            interface: Some(crate::endpoint::Interface::AnthropicMessages),
            prompt: "repair",
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
        let tools = r#"
import os
import json
from openjiuwen.core.foundation.tool.schema import ToolOutput
class _Tool:
    label = ""
    def __init__(self):
        self._renders = 0
    async def invoke(self, inputs, **kwargs):
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
async def main():
    agent, _tracker = factory.create_agent(None)
    assert agent._deep_config.enable_read_image_multimodal is False
    assert agent._deep_config.parallel_tool_calls is False
    result = []
    for tool in (ReadFileTool, WriteFileTool, EditFileTool, GlobTool, ListDirTool, GrepTool, BashTool, CodeTool):
        instance = tool()
        output = await instance.invoke({{"value": os.path.join(os.environ["ASB_TEST_WORKSPACE"], tool.label)}})
        # The real OpenJiuwen pipeline may reconstruct a ToolOutput between
        # invocation and rendering. Exercise the adapter's stable value key,
        # rather than relying on Python object identity.
        output = type(output).model_validate(output.model_dump())
        first_render = instance.render_for_llm(output)
        instance.render_for_llm(output)
        result.append(first_render)
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
        assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 8);
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
        std::fs::remove_dir_all(root).unwrap();
    }
}
