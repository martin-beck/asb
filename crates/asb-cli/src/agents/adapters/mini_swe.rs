use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, PreparationContext,
    StaticAdapter,
};
use crate::{Error, install::Recipe};
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "mini-swe",
    "mini",
    Recipe::Python {
        package: "mini-swe-agent==2.4.6",
        dependencies: &["mcp==2.3.0"],
        executable: "mini",
    },
    &[crate::endpoint::CHAT, crate::endpoint::MESSAGES_KEY],
    command,
    prepare_adapter,
)
.with_tool_replay(prepare_tool_replay)
.with_guest_private_assets(&[super::super::GuestPrivateAsset {
    target: "asb_mini_swe_tools.py",
    bytes: TOOL_TRANSPORT.as_bytes(),
}])
.with_artifact_specs(&[
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "local-tools.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "mini-swe",
        filename: "trajectory.json",
        format: super::super::ArtifactFormat::Json,
    },
])
.with_collection(collect)
.with_output_usage(output_usage)
.with_final_response(no_stdout_final_response);

// The final answer comes from the submitted trajectory, not arbitrary stdout.
fn no_stdout_final_response(_: &str, _: &std::path::Path) -> Option<String> {
    None
}

fn submitted_answer(document: &serde_json::Value) -> Option<String> {
    if document.get("trajectory_format")?.as_str()? != "mini-swe-agent-1.1"
        || document.pointer("/info/exit_status")?.as_str()? != "Submitted"
    {
        return None;
    }
    document
        .pointer("/info/submission")?
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

// Mini-SWE prints task and tool output as ordinary text; token-shaped data
// in that stream has no authoritative usage boundary. Its trajectory and the
// provider cassette are the structured sources of usage.
fn output_usage(_: &str, _: &str, _: bool) -> crate::measurements::TokenUsage {
    Default::default()
}
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "mini".into(),
        args: vec![
            "--model".into(),
            format!(
                "{}/{}",
                match context.interface {
                    Some(crate::endpoint::Interface::OpenAiChat) => "openai",
                    _ => "anthropic",
                },
                context.config.model
            ),
            "--task".into(),
            context.prompt.into(),
            "--yolo".into(),
            "--exit-immediately".into(),
        ],
        prompt_delivery: super::PromptDelivery::Embedded,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    if let Some(tape) = context.tape
        && tape.mode == crate::tape::Mode::Record
    {
        prepare_tool_transport(context, tape)?;
    }
    context
        .command
        .arg("--output")
        .arg(context.run_dir.join("mini-swe/trajectory.json"));
    let connection = context.connection;
    let (family, prefix) = match connection.interface {
        crate::endpoint::Interface::AnthropicMessages => ("ANTHROPIC", "anthropic"),
        crate::endpoint::Interface::OpenAiChat => ("OPENAI", "openai"),
        _ => return Err(context.unsupported_endpoint("mini-swe")),
    };
    let base_url = if connection.interface == crate::endpoint::Interface::AnthropicMessages {
        connection.base_url.trim_end_matches("/v1")
    } else {
        connection.base_url.as_str()
    };
    context
        .command
        .env(
            format!("{family}_API_KEY"),
            connection.credential.as_deref().unwrap_or("asb-local"),
        )
        .env(format!("{family}_BASE_URL"), base_url);
    context
        .command
        .env("MSWEA_CONFIGURED", "true")
        .env("MSWEA_MODEL_NAME", format!("{prefix}/{}", context.model))
        .env("MSWEA_SILENT_STARTUP", "1")
        .env("MSWEA_COST_TRACKING", "ignore_errors");
    Ok(())
}

/// Mini-SWE executes its own `bash` tool rather than accepting an MCP tool
/// server.  Keep that client protocol here: the generic tape only stages the
/// opaque sidecar selected for this run.
fn prepare_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    prepare_tool_transport(context, tape)
}

fn prepare_tool_transport(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    // Private inputs are staged at the guest-private root; `runtime_dir` is
    // its per-agent-state child.  Keep the import path aligned with the flat,
    // validated private-input target above.
    let directory = context.runtime_dir.parent().unwrap_or(context.runtime_dir);
    let transcript = match tape.mode {
        crate::tape::Mode::Record => {
            let transcript = context.run_dir.join("tool-replay/local-tools.jsonl");
            if let Some(parent) = transcript.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    Error::Run(format!(
                        "could not create Mini-SWE tool transcript directory: {error}"
                    ))
                })?;
            }
            std::fs::write(&transcript, b"").map_err(|error| {
                Error::Run(format!(
                    "could not initialize Mini-SWE tool transcript: {error}"
                ))
            })?;
            transcript
        }
        // The generic guest boundary stages the selected opaque replay
        // sidecar at its private root. Mini-SWE owns its filename and uses it
        // only through this adapter-local environment extension.
        crate::tape::Mode::Replay => directory.join("local-tools.jsonl"),
    };
    context
        .command
        .arg("--environment-class")
        .arg("asb_mini_swe_tools.AsbTapeEnvironment")
        .env("PYTHONPATH", directory)
        .env("ASB_MINI_SWE_TOOL_TAPE", transcript)
        .env(
            "ASB_MINI_SWE_TOOL_MODE",
            match tape.mode {
                crate::tape::Mode::Record => "record",
                crate::tape::Mode::Replay => "replay",
            },
        );
    Ok(())
}

/// Adapter-private Mini-SWE environment extension.  It serializes only the
/// agent's own command/result exchange; the sealed artifact pipeline retains
/// the bytes as an opaque local-tools sidecar.
const TOOL_TRANSPORT: &str = r#"import hashlib
import json
import os
from pathlib import Path

from minisweagent.environments.local import LocalEnvironment
from minisweagent.exceptions import Submitted

_PATH = Path(os.environ["ASB_MINI_SWE_TOOL_TAPE"])
_MODE = os.environ["ASB_MINI_SWE_TOOL_MODE"]
_LIMIT = 128
_MAX_EFFECT_BYTES = 512 * 1024


class AsbTapeEnvironment(LocalEnvironment):
    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        if _MODE == "replay":
            self._entries = [json.loads(line) for line in _PATH.read_text(encoding="utf-8").splitlines() if line]
        else:
            self._entries = []
        if len(self._entries) > _LIMIT:
            raise RuntimeError("Mini-SWE local tool transcript exceeds the bounded replay limit")
        self._position = 0

    def _append(self, entry):
        if len(self._entries) >= _LIMIT:
            raise RuntimeError("Mini-SWE local tool transcript exceeds the bounded replay limit")
        encoded = json.dumps(entry, separators=(",", ":"), ensure_ascii=False)
        with _PATH.open("a", encoding="utf-8") as handle:
            handle.write(encoded + "\n")
        self._entries.append(entry)

    def _record(self, command, result, effects):
        event = {
            "schema_version": 1,
            "sequence": len(self._entries) + 1,
            "tool": "mini-swe.bash",
            "request": {"command": command},
            "result": result,
        }
        if effects:
            event["effects"] = effects
        self._append(event)

    def _record_failure(self, command, failure):
        # `_effects` deliberately refuses oversized or non-text mutations.
        # The rejected call remains causally observable: otherwise replay
        # advances its provider tape and asks for a later, unrecorded tool
        # result.  `failure` is selected only from this extension's bounded
        # static errors, never from command output or a host exception.
        event = {
            "schema_version": 1,
            "sequence": len(self._entries) + 1,
            "tool": "mini-swe.bash",
            "request": {"command": command},
            "result": None,
            "failure": failure,
        }
        self._append(event)

    def _root(self, cwd):
        root = Path(cwd or self.config.cwd or os.getcwd()).resolve()
        if not root.is_dir():
            raise RuntimeError("Mini-SWE local tool workspace is unavailable")
        return root

    def _snapshot(self, root):
        files = {}
        for path in root.rglob("*"):
            if not path.is_file() or path.is_symlink():
                continue
            if any(part in {".git", "__pycache__"} for part in path.parts) or path.suffix in {".pyc", ".pyo"}:
                continue
            relative = path.relative_to(root).as_posix()
            with path.open("rb") as source:
                files[relative] = hashlib.file_digest(source, "sha256").digest()
        return files

    def _effects(self, root, before, after):
        effects = []
        total = 0
        for path in sorted(before.keys() - after.keys()):
            effects.append({"kind": "remove_file", "path": path})
        for path in sorted(after):
            if before.get(path) != after[path]:
                try:
                    content = (root / path).read_text(encoding="utf-8")
                except UnicodeDecodeError:
                    raise RuntimeError("Mini-SWE local tool changed a non-text workspace file")
                total += len(content.encode("utf-8"))
                if total > _MAX_EFFECT_BYTES:
                    raise RuntimeError("Mini-SWE local tool workspace effects exceed the bounded replay limit")
                effects.append({"kind": "write_text", "path": path, "content": content})
        return effects

    def _apply_effects(self, root, effects):
        if not isinstance(effects, list):
            raise RuntimeError("Mini-SWE replay workspace effects are malformed")
        for effect in effects:
            if not isinstance(effect, dict):
                raise RuntimeError("Mini-SWE replay workspace effect is malformed")
            path = effect.get("path")
            if not isinstance(path, str) or not path or Path(path).is_absolute() or ".." in Path(path).parts:
                raise RuntimeError("Mini-SWE replay workspace effect escapes its workspace")
            target = root.joinpath(*Path(path).parts)
            kind = effect.get("kind")
            if kind == "write_text":
                content = effect.get("content")
                if not isinstance(content, str):
                    raise RuntimeError("Mini-SWE replay workspace write is malformed")
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(content, encoding="utf-8")
            elif kind == "remove_file":
                if target.is_file() or target.is_symlink():
                    target.unlink()
            else:
                raise RuntimeError("Mini-SWE replay workspace effect is unsupported")

    def execute(self, action, cwd="", *, timeout=None):
        command = action.get("command", "")
        if _MODE == "record":
            root = self._root(cwd)
            before = self._snapshot(root)
            try:
                output = super().execute(action, cwd, timeout=timeout)
            except Submitted as submitted:
                try:
                    effects = self._effects(root, before, self._snapshot(root))
                except RuntimeError as error:
                    self._record_failure(command, str(error))
                    raise
                self._record(command, {"submitted": list(submitted.messages)}, effects)
                raise
            try:
                effects = self._effects(root, before, self._snapshot(root))
            except RuntimeError as error:
                self._record_failure(command, str(error))
                raise
            self._record(command, output, effects)
            return output
        if self._position >= len(self._entries):
            raise RuntimeError("Mini-SWE replay requested an unrecorded local tool result")
        entry = self._entries[self._position]
        self._position += 1
        if (
            entry.get("schema_version") != 1
            or entry.get("sequence") != self._position
            or entry.get("tool") != "mini-swe.bash"
            or entry.get("request", {}).get("command") != command
        ):
            raise RuntimeError("Mini-SWE replay local tool command differs from the recording")
        failure = entry.get("failure")
        if failure is not None:
            if (
                not isinstance(failure, str)
                or failure not in {
                    "Mini-SWE local tool changed a non-text workspace file",
                    "Mini-SWE local tool workspace effects exceed the bounded replay limit",
                }
                or entry.get("result") is not None
                or entry.get("effects")
            ):
                raise RuntimeError("Mini-SWE replay local tool failure is malformed")
            raise RuntimeError(failure)
        output = entry.get("result")
        if not isinstance(output, dict):
            raise RuntimeError("Mini-SWE replay local tool result is malformed")
        self._apply_effects(self._root(cwd), entry.get("effects", []))
        if "submitted" in output:
            submitted = output["submitted"]
            if not isinstance(submitted, list):
                raise RuntimeError("Mini-SWE replay submission is malformed")
            raise Submitted(*submitted)
        return output
"#;

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let Some(document) = context.frozen.document("trajectory.json") else {
        return AgentArtifacts::default();
    };
    AgentArtifacts {
        final_response: submitted_answer(document),
        token_usage: Some(trajectory_usage(document)),
        steps: trajectory_steps(document),
        tool_call_count: trajectory_tool_calls(document),
        ..Default::default()
    }
}

fn trajectory_tool_calls(value: &serde_json::Value) -> Option<usize> {
    if value.get("trajectory_format")?.as_str()? != "mini-swe-agent-1.1" {
        return None;
    }
    let messages = value.get("messages")?.as_array()?;
    if messages.last()?.get("role")?.as_str()? != "exit" {
        return None;
    }
    messages.iter().try_fold(0usize, |sum, message| {
        let role = message.get("role")?.as_str()?;
        // Format errors are recorded as user messages carrying the provider
        // response; rejected calls may be present there rather than in an
        // assistant message, so the assistant-only count would be partial.
        if message
            .pointer("/extra/interrupt_type")
            .and_then(|v| v.as_str())
            == Some("FormatError")
        {
            return None;
        }
        if role != "assistant" {
            return Some(sum);
        }
        let calls = message.get("tool_calls")?.as_array()?;
        sum.checked_add(calls.len())
    })
}

fn trajectory_steps(value: &serde_json::Value) -> Option<u64> {
    // mini-SWE 2.4.6 increments this counter in query() and uses it for its
    // configured step limit. It includes failed model-call attempts, unlike
    // the number of successful response/usage records in the trajectory.
    if value.get("trajectory_format")?.as_str()? != "mini-swe-agent-1.1" {
        return None;
    }
    value.pointer("/info/model_stats/api_calls")?.as_u64()
}

fn trajectory_usage(value: &serde_json::Value) -> crate::measurements::TokenUsage {
    let Some(messages) = value.get("messages").and_then(|v| v.as_array()) else {
        // Retain the legacy compact artifact format, but do not recursively
        // interpret arbitrary nested task content as model usage.
        let direct = super::super::usage_fields(value);
        let nested = value
            .get("usage")
            .map(crate::usage::usage_from_json)
            .unwrap_or_default();
        return direct.merge(nested).with_derived_total();
    };
    let mut sum = crate::measurements::TokenUsage::default();
    let mut seen = false;
    let mut cache_complete = true;
    for message in messages {
        let Some(response) = message.pointer("/extra/response") else {
            continue;
        };
        let Some(value) = response.get("usage") else {
            return Default::default();
        };
        let usage = crate::usage::usage_from_json(value);
        if usage.input_tokens.is_none() || usage.output_tokens.is_none() {
            return Default::default();
        }
        cache_complete &= usage.cached_tokens.is_some();
        sum = sum.add(usage);
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

#[cfg(test)]
mod tests {
    #[test]
    fn failed_trajectory_does_not_grade_last_tool_output() {
        let doc = serde_json::json!({"trajectory_format":"mini-swe-agent-1.1","info":{"exit_status":"LimitsExceeded","submission":""},"messages":[{"role":"user","content":"tool output"}]});
        let frozen = super::super::super::FrozenArtifacts::for_test("trajectory.json", doc);
        let context = super::CollectionContext {
            frozen: &frozen,
            provider_capture: None,
            run_dir: std::path::Path::new("."),
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
        };
        assert!(super::collect(&context).final_response.is_none());
        let successful = serde_json::json!({"trajectory_format":"mini-swe-agent-1.1","info":{"exit_status":"Submitted","submission":"final answer"},"messages":[{"role":"user","content":"tool output"}]});
        let frozen = super::super::super::FrozenArtifacts::for_test("trajectory.json", successful);
        let context = super::CollectionContext {
            frozen: &frozen,
            ..context
        };
        assert_eq!(
            super::collect(&context).final_response.as_deref(),
            Some("final answer")
        );
        assert!(
            super::no_stdout_final_response(
                r#"{"type":"final","text":"tool output"}"#,
                std::path::Path::new(".")
            )
            .is_none()
        );
    }

    #[test]
    fn tool_count_uses_only_assistant_trajectory_calls() {
        let doc = serde_json::json!({"trajectory_format":"mini-swe-agent-1.1","messages":[
            {"role":"user", "content":"secret tool_calls"},
            {"role":"assistant", "tool_calls":[{"function":{"arguments":"sensitive"}}]},
            {"role":"tool", "content":"secret"},
            {"role":"assistant", "tool_calls":[{}, {}]},
            {"role":"exit"}
        ]});
        assert_eq!(super::trajectory_tool_calls(&doc), Some(3));
        let empty = serde_json::json!({"trajectory_format":"mini-swe-agent-1.1","messages":[{"role":"exit"}]});
        assert_eq!(super::trajectory_tool_calls(&empty), Some(0));
        let malformed = serde_json::json!({"trajectory_format":"mini-swe-agent-1.1","messages":[{"role":"assistant","tool_calls":"invalid"},{"role":"exit"}]});
        assert_eq!(super::trajectory_tool_calls(&malformed), None);
        let missing = serde_json::json!({"trajectory_format":"mini-swe-agent-1.1","messages":[{"role":"assistant"},{"role":"exit"}]});
        assert_eq!(super::trajectory_tool_calls(&missing), None);
        let unfinished = serde_json::json!({"trajectory_format":"mini-swe-agent-1.1","messages":[{"role":"assistant","tool_calls":[{}]}]});
        assert_eq!(super::trajectory_tool_calls(&unfinished), None);
        let rejected_then_success = serde_json::json!({"trajectory_format":"mini-swe-agent-1.1","messages":[{"role":"user","extra":{"interrupt_type":"FormatError","response":{"choices":[{"message":{"tool_calls":[{}]}}]}}},{"role":"assistant","tool_calls":[{}]},{"role":"exit"}]});
        assert_eq!(super::trajectory_tool_calls(&rejected_then_success), None);
        assert_eq!(
            super::trajectory_tool_calls(
                &serde_json::json!({"trajectory_format":"unknown","messages":[]})
            ),
            None
        );
    }
    use super::*;
    use crate::agents::AgentAdapter;
    use std::{collections::HashMap, process::Command};

    #[test]
    fn stdout_task_json_is_not_usage_when_trajectory_has_no_usage() {
        let task_output = r#"{"usage":{"input_tokens":999,"output_tokens":888}}"#;
        let from_output =
            crate::usage::from_sources("openrouter", "mini-swe", task_output, "", None);
        let trajectory = trajectory_usage(
            &serde_json::json!({"trajectory_format":"mini-swe-agent-1.1","messages":[]}),
        );
        let usage = crate::usage::with_artifact_usage(from_output, Some(trajectory));
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (None, None, None)
        );
    }

    #[test]
    fn trajectory_task_content_is_not_provider_usage() {
        let trajectory = serde_json::json!({"trajectory_format":"mini-swe-agent-1.1", "messages":[
            {"role":"user","content":"{\"usage\":{\"input_tokens\":999,\"output_tokens\":888}}"}
        ]});
        let usage = trajectory_usage(&trajectory);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (None, None, None)
        );
        let malformed =
            serde_json::json!({"task":{"usage":{"input_tokens":999,"output_tokens":888}}});
        assert_eq!(trajectory_usage(&malformed).total_tokens, None);
        let legacy = serde_json::json!({"input_tokens":13,"output_tokens":21});
        assert_eq!(trajectory_usage(&legacy).total_tokens, Some(34));
        let stale_total =
            serde_json::json!({"input_tokens":182398,"output_tokens":4189,"total_tokens":116});
        assert_eq!(trajectory_usage(&stale_total).total_tokens, Some(186587));
        let legacy_wrapped = serde_json::json!({"usage":{"input_tokens":13,"output_tokens":21}});
        assert_eq!(trajectory_usage(&legacy_wrapped).total_tokens, Some(34));
    }

    fn environment(command: &Command) -> HashMap<String, String> {
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
    fn tape_backed_preparation_owns_mini_swe_bash_results() {
        let root = std::env::temp_dir().join(format!("asb-mini-swe-tape-{}", std::process::id()));
        let workspace = root.join("workspace");
        let run_dir = root.join("run");
        std::fs::create_dir_all(&workspace).unwrap();
        let cassette = root.join("record.jsonl");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("fixture-key".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let runtime = root.join("agent-state");
        let mut command = Command::new("mini");
        ADAPTER
            .prepare(&mut PreparationContext {
                command: &mut command,
                model: "fixture-model",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &runtime,
                run_dir: &run_dir,
                mcp_servers: &[],
                mcp_tape: None,
                tape: Some(super::super::TapeContext {
                    mode: crate::tape::Mode::Record,
                    cassette: &cassette,
                }),
            })
            .unwrap();
        let arguments = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(arguments.windows(2).any(|pair| {
            pair[0] == "--environment-class" && pair[1] == "asb_mini_swe_tools.AsbTapeEnvironment"
        }));
        let command_environment = environment(&command);
        let transcript = run_dir.join("tool-replay/local-tools.jsonl");
        assert_eq!(
            command_environment.get("ASB_MINI_SWE_TOOL_TAPE"),
            Some(&transcript.to_string_lossy().into_owned())
        );
        assert_eq!(
            command_environment.get("ASB_MINI_SWE_TOOL_MODE"),
            Some(&"record".into())
        );
        assert_eq!(
            command_environment.get("PYTHONPATH"),
            Some(&root.to_string_lossy().into_owned())
        );
        assert!(transcript.is_file());

        let mut replay_command = Command::new("mini");
        ADAPTER
            .prepare(&mut PreparationContext {
                command: &mut replay_command,
                model: "fixture-model",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &runtime,
                run_dir: &run_dir,
                mcp_servers: &[],
                mcp_tape: None,
                tape: Some(super::super::TapeContext {
                    mode: crate::tape::Mode::Replay,
                    cassette: &cassette,
                }),
            })
            .unwrap();
        let replay_environment = environment(&replay_command);
        assert_eq!(
            replay_environment.get("ASB_MINI_SWE_TOOL_TAPE"),
            Some(
                &root
                    .join("local-tools.jsonl")
                    .to_string_lossy()
                    .into_owned()
            )
        );
        assert_eq!(
            replay_environment.get("ASB_MINI_SWE_TOOL_MODE"),
            Some(&"replay".into())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mini_swe_tool_tape_uses_real_jsonl_separators() {
        // `TOOL_TRANSPORT` is a Rust raw string. The embedded Python must
        // receive one escape, not a literal backslash followed by `n`, or
        // adjacent records become one malformed JSON document at sealing.
        assert!(TOOL_TRANSPORT.contains("handle.write(encoded + \"\\n\")"));
        assert!(!TOOL_TRANSPORT.contains("handle.write(encoded + \"\\\\n\")"));
    }

    #[test]
    fn mini_swe_replays_the_client_submission_payload_without_retyping_it() {
        assert!(TOOL_TRANSPORT.contains("if not isinstance(submitted, list):"));
        assert!(!TOOL_TRANSPORT.contains("all(isinstance(item, str) for item in submitted)"));
    }

    #[test]
    fn mini_swe_seals_bounded_workspace_effect_failures_for_exact_replay() {
        assert!(TOOL_TRANSPORT.contains("def _record_failure(self, command, failure):"));
        assert!(TOOL_TRANSPORT.contains("self._record_failure(command, str(error))"));
        assert!(TOOL_TRANSPORT.contains("Mini-SWE replay local tool failure is malformed"));
        assert!(
            TOOL_TRANSPORT
                .contains("Mini-SWE local tool workspace effects exceed the bounded replay limit")
        );

        let transcript = br#"{"schema_version":1,"sequence":1,"tool":"mini-swe.bash","request":{"command":"true"},"result":null,"failure":"Mini-SWE local tool workspace effects exceed the bounded replay limit"}
"#;
        assert_eq!(
            crate::agents::tool_replay::Transcript::decode(transcript)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn collection_uses_the_native_step_limit_counter() {
        let workspace =
            std::env::temp_dir().join(format!("asb-mini-swe-steps-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).unwrap();
        let trajectory = workspace.join(".asb-mini-swe.traj.json");
        for (document, expected) in [
            (
                r#"{"trajectory_format":"mini-swe-agent-1.1","info":{"model_stats":{"api_calls":4}},"messages":[{"role":"assistant"}]}"#,
                Some(4),
            ),
            (
                r#"{"trajectory_format":"mini-swe-agent-1.1","info":{"model_stats":{"api_calls":0}}}"#,
                Some(0),
            ),
            (
                r#"{"trajectory_format":"mini-swe-agent-1.1","info":{"model_stats":{"api_calls":-1}}}"#,
                None,
            ),
            (
                r#"{"trajectory_format":"mini-swe-agent-1.1","info":{"model_stats":{"api_calls":"4"}}}"#,
                None,
            ),
            (
                r#"{"trajectory_format":"unknown","info":{"model_stats":{"api_calls":4}}}"#,
                None,
            ),
            (r#"{"api_calls":4,"messages":[{"role":"assistant"}]}"#, None),
            ("invalid", None),
        ] {
            std::fs::write(&trajectory, document).unwrap();
            let artifacts = collect(&CollectionContext {
                provider_capture: None,
                frozen: &serde_json::from_str(document)
                    .ok()
                    .map(|value| {
                        super::super::super::FrozenArtifacts::for_test("trajectory.json", value)
                    })
                    .unwrap_or_default(),
                run_dir: &workspace,
                interface: None,
                anthropic_input_includes_cache: false,
                mcp_servers: &[],
                mcp_tape: None,
            });
            assert_eq!(artifacts.steps, expected, "{document}");
        }
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn collection_owns_the_mini_swe_trajectory_artifact() {
        let workspace =
            std::env::temp_dir().join(format!("asb-mini-swe-artifact-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).expect("workspace exists");
        let trajectory = workspace.join(".asb-mini-swe.traj.json");
        std::fs::write(&trajectory, r#"{"input_tokens": 13, "output_tokens": 21}"#)
            .expect("trajectory exists");

        let artifacts = collect(&CollectionContext {
            provider_capture: None,
            frozen: &super::super::super::FrozenArtifacts::for_test(
                "trajectory.json",
                serde_json::json!({"input_tokens":13,"output_tokens":21}),
            ),
            run_dir: &workspace,
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
        });

        assert_eq!(
            artifacts
                .token_usage
                .as_ref()
                .and_then(|usage| usage.input_tokens),
            Some(13)
        );
        let _ = std::fs::remove_dir_all(workspace);
    }
}
