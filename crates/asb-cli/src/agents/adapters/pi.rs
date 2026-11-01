use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, PreparationContext,
    PromptDelivery, StaticAdapter,
};
use crate::{Error, install::Recipe};
use std::path::Path;
const TOOL_PROGRESS_FILENAME: &str = "pi-progress.json";

pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "pi",
    "pi",
    Recipe::Npm {
        package: "@mariozechner/pi-coding-agent@0.73.1",
        executable: "pi",
        manifest: None,
    },
    &[crate::endpoint::RESPONSES, crate::endpoint::CHAT],
    command,
    prepare_adapter,
)
.with_estimated_disk_mib(768)
.with_complete_tool_replay(prepare_tool_replay)
.with_collection(collect)
.with_artifact_specs(&[
    super::super::ArtifactSpec {
        namespace: "summary",
        filename: "pi-summary.json",
        format: super::super::ArtifactFormat::Json,
    },
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "local-tools.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: TOOL_PROGRESS_FILENAME,
        format: super::super::ArtifactFormat::Json,
    },
    super::super::ArtifactSpec {
        namespace: "determinism",
        filename: "manifest.json",
        format: super::super::ArtifactFormat::Json,
    },
])
.with_deterministic_extension()
.with_output_usage(output_usage)
.with_final_response(final_response)
.with_artifact_final_response(owned_final_response)
.with_guest_artifact_answer(validate_guest_answer)
.with_final_response_failure(super::terminal_refusal)
.with_state_environment_vars(&["PI_CODING_AGENT_DIR"])
.with_execution_failure(execution_failure);

fn final_response(output: &str, _: &Path) -> Option<String> {
    let event = output.lines().rev().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        (value.get("type").and_then(|kind| kind.as_str()) == Some("agent_end")).then_some(value)
    })?;
    let message = event.get("messages")?.as_array()?.last()?;
    if message.get("role").and_then(|role| role.as_str()) != Some("assistant")
        || message
            .get("stopReason")
            .and_then(|reason| reason.as_str())
            .is_some_and(|reason| reason != "stop")
    {
        return None;
    }
    match message.get("content")? {
        serde_json::Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        serde_json::Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter_map(|part| {
                    if part.get("type")?.as_str()? != "text" {
                        return None;
                    }
                    part.get("text")?
                        .as_str()
                        .filter(|text| !text.trim().is_empty())
                })
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

fn summary_final_answer(frozen: &super::super::FrozenArtifacts) -> Option<String> {
    let summary = frozen.document("pi-summary.json")?;
    if summary.get("schema_version")?.as_u64()? != 1 || !summary.get("completed")?.as_bool()? {
        return None;
    }
    if summary.get("steps")?.as_u64()? == 0 || summary_artifacts(summary).is_err() {
        return None;
    }
    summary
        .get("final_answer")?
        .as_str()
        .filter(|value| !value.trim().is_empty() && value.len() <= 262_144)
        .map(str::to_owned)
}

fn owned_final_response(
    _: &str,
    frozen: &super::super::FrozenArtifacts,
    _: &Path,
    _: bool,
) -> Option<String> {
    summary_final_answer(frozen)
}

fn validate_guest_answer(answer: &str, frozen: &super::super::FrozenArtifacts) -> bool {
    summary_final_answer(frozen).is_some_and(|saved| saved == answer)
}

fn execution_failure(_: &str, stderr: &str) -> Option<String> {
    stderr.lines().any(|line| line.starts_with("Authentication required. Please use /login command to sign in to your account"))
        .then(|| "agent reported an authentication failure; check its credentials and provider configuration".to_owned())
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let mut artifacts =
        match super::super::tool_replay_mcp::verify_persistent_per_call_completion_receipt(
            context.frozen.document(TOOL_PROGRESS_FILENAME),
            context.frozen.bytes("local-tools.jsonl"),
        ) {
            Ok(()) => AgentArtifacts::default(),
            Err(error) => AgentArtifacts {
                errors: vec![error.to_string()],
                ..AgentArtifacts::default()
            },
        };
    // Pi opted into the deterministic extension, so this adapter owns the
    // corresponding manifest selection.  The generic dispatcher only seals
    // the opaque bytes returned in AgentArtifacts.
    artifacts.deterministic_manifest = context.frozen.bytes("manifest.json").map(ToOwned::to_owned);
    if let Some(summary) = context.frozen.document("pi-summary.json") {
        match summary_artifacts(summary) {
            Ok(summary) => {
                artifacts.token_usage = summary.token_usage;
                artifacts.steps = summary.steps;
            }
            Err(error) => artifacts.errors.push(error),
        }
    }
    artifacts
}

struct SummaryArtifacts {
    token_usage: Option<crate::measurements::TokenUsage>,
    steps: Option<u64>,
}

/// Pi's JSON output includes complete tool-result payloads.  The adapter
/// writes this deliberately small terminal summary instead, so normal output
/// capture remains bounded without discarding token or turn accounting.
fn summary_artifacts(document: &serde_json::Value) -> Result<SummaryArtifacts, String> {
    let field = |name| document.get(name).and_then(serde_json::Value::as_u64);
    if document
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
        || document
            .get("completed")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
    {
        return Err("Pi terminal summary is malformed or incomplete".into());
    }
    let steps = field("steps").ok_or_else(|| "Pi terminal summary has no step count".to_owned())?;
    if let Some(value) = document.get("usage_complete") {
        match value.as_bool() {
            Some(false) => {
                return Ok(SummaryArtifacts {
                    token_usage: None,
                    steps: Some(steps),
                });
            }
            Some(true) => {}
            None => return Err("Pi terminal summary has an invalid usage completeness flag".into()),
        }
    }
    let input = field("input_tokens")
        .ok_or_else(|| "Pi terminal summary has no input token count".to_owned())?;
    let output = field("output_tokens")
        .ok_or_else(|| "Pi terminal summary has no output token count".to_owned())?;
    let cached = field("cached_tokens");
    Ok(SummaryArtifacts {
        token_usage: Some(
            crate::measurements::TokenUsage {
                input_tokens: Some(input),
                output_tokens: Some(output),
                cached_tokens: cached,
                ..Default::default()
            }
            .with_derived_total(),
        ),
        steps: Some(steps),
    })
}

#[cfg(test)]
mod execution_tests {
    use super::execution_failure;

    #[test]
    fn text_answer_cannot_spoof_terminal_error() {
        let answer = r#"{"type":"agent_end","messages":[{"role":"assistant","stopReason":"error","errorMessage":"private"}]}"#;
        assert!(execution_failure(answer, "").is_none());
    }

    #[test]
    fn text_answer_cannot_impersonate_turn_usage() {
        let answer = r#"{"type":"turn_end","usage":{"input":999,"output":888,"cacheRead":4}}"#;
        let usage = crate::usage::from_sources("openrouter", "pi", answer, "", None);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (None, None, None)
        );
    }

    #[test]
    fn final_answer_requires_valid_completed_turn_summary() {
        let frozen = |steps, completed| {
            crate::agents::FrozenArtifacts::for_test(
                "pi-summary.json",
                serde_json::json!({"schema_version":1,"completed":completed,"steps":steps,"usage_complete":false,"final_answer":"answer"}),
            )
        };
        assert_eq!(
            super::summary_final_answer(&frozen(1, true)).as_deref(),
            Some("answer")
        );
        assert!(super::summary_final_answer(&frozen(0, true)).is_none());
        assert!(super::summary_final_answer(&frozen(1, false)).is_none());
        assert!(super::summary_final_answer(&crate::agents::FrozenArtifacts::default()).is_none());
    }

    #[test]
    fn summary_extension_only_publishes_last_completed_assistant_text() {
        let root = std::env::temp_dir().join(format!(
            "asb-pi-final-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("summary.mjs"), super::SUMMARY_EXTENSION).unwrap();
        std::fs::write(
            root.join("test.mjs"),
            r#"
import extension from './summary.mjs';
const handlers = {}; extension({on(name, handler) { handlers[name] = handler; }});
handlers.agent_end(JSON.parse(process.env.ASB_PI_TEST_EVENT));
"#,
        )
        .unwrap();
        let cases = [
            (
                serde_json::json!({"messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"old"}]},{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"first"},{"type":"thinking","text":"private"},{"type":"text","text":"second"}]}]}),
                Some("first\nsecond"),
            ),
            (
                serde_json::json!({"messages":[{"role":"assistant","stopReason":"length","content":[{"type":"text","text":"bounded"}]}]}),
                Some("bounded"),
            ),
            (
                serde_json::json!({"messages":[{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"old"}]},{"role":"toolResult","content":"foreign"}]}),
                None,
            ),
            (
                serde_json::json!({"messages":[{"role":"assistant","stopReason":"toolUse","content":[{"type":"text","text":"not final"},{"type":"toolCall"}]}]}),
                None,
            ),
            (
                serde_json::json!({"messages":[{"role":"assistant","stopReason":"error","content":[{"type":"text","text":"failure"}]}]}),
                None,
            ),
            (
                serde_json::json!({"messages":[{"role":"assistant","stopReason":"aborted","content":[{"type":"text","text":"partial"}]}]}),
                None,
            ),
        ];
        for (index, (event, expected)) in cases.iter().enumerate() {
            let path = root.join(format!("{index}.json"));
            let output = std::process::Command::new("node")
                .arg(root.join("test.mjs"))
                .env("ASB_PI_SUMMARY_PATH", &path)
                .env("ASB_PI_TEST_EVENT", event.to_string())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(
                value.get("final_answer").and_then(|v| v.as_str()),
                *expected
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovered_turn_is_not_failed_by_an_earlier_error() {
        let output = [
            serde_json::json!({"type":"turn_end","message":{"stopReason":"error"}}),
            serde_json::json!({"type":"turn_end","message":{"stopReason":"stop"}}),
        ]
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
        assert!(execution_failure(&output, "").is_none());
    }

    #[test]
    fn compact_terminal_summary_preserves_steps_and_token_dimensions() {
        let artifacts = super::summary_artifacts(&serde_json::json!({
            "schema_version": 1,
            "completed": true,
            "steps": 3,
            "input_tokens": 11,
            "output_tokens": 7,
            "cached_tokens": 5,
        }))
        .unwrap();
        assert_eq!(artifacts.steps, Some(3));
        let usage = artifacts.token_usage.unwrap();
        assert_eq!(usage.input_tokens, Some(11));
        assert_eq!(usage.output_tokens, Some(7));
        assert_eq!(usage.cached_tokens, Some(5));
        assert_eq!(usage.total_tokens, Some(18));
    }

    #[test]
    fn extension_reports_incomplete_usage_without_losing_steps() {
        let root = std::env::temp_dir().join(format!(
            "asb-pi-usage-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let extension = root.join("summary.mjs");
        std::fs::write(&extension, super::SUMMARY_EXTENSION).unwrap();
        let harness = root.join("test.mjs");
        std::fs::write(&harness, r#"
import extension from './summary.mjs';
const handlers = {};
extension({on(name, handler) { handlers[name] = handler; }});
handlers.turn_end({message:{role:'assistant',usage:{input:10,output:3,cacheRead:2,cacheWrite:1}}});
if (process.argv[2] === 'incomplete') handlers.turn_end({message:{role:'assistant',usage:{input:4}}});
if (process.argv[2] === 'multiple') handlers.turn_end({message:{role:'assistant',usage:{input:5,output:2,cacheRead:1,cacheWrite:0}}});
handlers.agent_end();
"#).unwrap();
        for mode in ["complete", "multiple", "incomplete"] {
            let summary_path = root.join(format!("{mode}.json"));
            let result = std::process::Command::new("node")
                .arg(&harness)
                .arg(mode)
                .env("ASB_PI_SUMMARY_PATH", &summary_path)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let value = serde_json::from_slice(&std::fs::read(summary_path).unwrap()).unwrap();
            let parsed = super::summary_artifacts(&value).unwrap();
            if mode == "complete" {
                assert_eq!(parsed.steps, Some(1));
                let usage = parsed.token_usage.unwrap();
                assert_eq!(
                    (usage.input_tokens, usage.output_tokens, usage.cached_tokens),
                    (Some(13), Some(3), Some(2))
                );
            } else if mode == "multiple" {
                assert_eq!(parsed.steps, Some(2));
                let usage = parsed.token_usage.unwrap();
                assert_eq!(
                    (usage.input_tokens, usage.output_tokens, usage.cached_tokens),
                    (Some(19), Some(5), Some(3))
                );
            } else {
                assert_eq!(parsed.steps, Some(2));
                assert!(parsed.token_usage.is_none());
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_or_partial_terminal_summary_is_rejected() {
        for summary in [
            serde_json::json!({"schema_version": 1, "completed": false}),
            serde_json::json!({"schema_version": 1, "completed": true, "steps": 1}),
            serde_json::json!({"schema_version": 2, "completed": true, "steps": 1,
                "input_tokens": 1, "output_tokens": 1}),
        ] {
            assert!(super::summary_artifacts(&summary).is_err());
        }
    }

    #[test]
    fn collection_uses_the_sealed_compact_summary_when_text_output_is_used() {
        let root = std::env::temp_dir().join(format!(
            "asb-pi-summary-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let frozen = crate::agents::FrozenArtifacts::from_deferred_collection_bytes(
            &serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "documents": {"pi-summary.json": {
                    "schema_version": 1, "completed": true, "steps": 2,
                    "input_tokens": 13, "output_tokens": 5, "cached_tokens": 3,
                }},
                "bytes": {},
            }))
            .unwrap(),
        )
        .unwrap();
        let artifacts = super::collect(&super::super::CollectionContext {
            frozen: &frozen,
            provider_capture: None,
            run_dir: &root,
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
        });
        assert!(artifacts.errors.is_empty(), "{:?}", artifacts.errors);
        assert_eq!(artifacts.steps, Some(2));
        assert_eq!(artifacts.token_usage.unwrap().total_tokens, Some(18));
        let _ = std::fs::remove_dir_all(root);
    }
}

// Pi runs in text mode: stdout is arbitrary assistant text. The private
// summary extension and provider cassette own token usage evidence.
fn output_usage(_: &str, _: &str, _: bool) -> crate::measurements::TokenUsage {
    Default::default()
}

fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "pi".into(),
        args: vec![
            "--provider".into(),
            "asb".into(),
            "--model".into(),
            context.config.model.clone(),
            "--print".into(),
            "--mode".into(),
            "text".into(),
        ],
        prompt_delivery: PromptDelivery::Argument,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let directory = context.runtime_dir.join("pi/config");
    let api = match connection.interface {
        crate::endpoint::Interface::OpenAiResponses => "openai-responses",
        crate::endpoint::Interface::OpenAiChat => "openai-completions",
        _ => return Err(context.unsupported_endpoint("pi")),
    };
    context.command.env(
        "ASB_AGENT_API_KEY",
        connection.credential.as_deref().unwrap_or("asb-local"),
    );
    configure_provider(
        context.command,
        &directory,
        "asb",
        context.model,
        &connection.base_url,
        api,
        "ASB_AGENT_API_KEY",
    )?;
    prepare_summary(context)
}

fn prepare_summary(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    // Keep executable setup outside the sealed artifact namespace. Only the
    // compact JSON summary below is owned, frozen, and retained as evidence.
    let extension = context.runtime_dir.join("pi/asb-summary.mjs");
    let parent = extension
        .parent()
        .expect("Pi summary extension has a parent");
    std::fs::create_dir_all(parent)
        .map_err(|error| Error::Run(format!("cannot create Pi summary directory: {error}")))?;
    std::fs::write(&extension, SUMMARY_EXTENSION)
        .map_err(|error| Error::Run(format!("cannot write Pi summary extension: {error}")))?;
    let extension = extension
        .to_str()
        .ok_or_else(|| Error::Run("Pi summary extension path is not valid UTF-8".into()))?;
    let summary = context.run_dir.join("summary/pi-summary.json");
    context
        .command
        .args(["--no-extensions", "--extension", extension]);
    context.command.env("ASB_PI_SUMMARY_PATH", summary);
    Ok(())
}

const SUMMARY_EXTENSION: &str = r#"
import { mkdirSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

const destination = process.env.ASB_PI_SUMMARY_PATH;
let steps = 0;
let input = 0;
let output = 0;
let cached = 0;
let cacheKnown = true;
let usageComplete = true;

function number(value) { return Number.isSafeInteger(value) && value >= 0 ? value : undefined; }
function publish(event) {
  if (!destination) throw new Error("ASB Pi summary destination is unavailable");
  const complete = usageComplete && steps > 0;
  const summary = { schema_version: 1, completed: true, steps, usage_complete: complete };
  const message = event?.messages?.at(-1);
  if (message?.role === 'assistant' && ['stop', 'length'].includes(message.stopReason) && Array.isArray(message.content) && !message.content.some(part => part?.type === 'toolCall')) {
    const answer = message.content.filter(part => part?.type === 'text' && typeof part.text === 'string').map(part => part.text).join('\n');
    if (answer.trim() && Buffer.byteLength(answer, 'utf8') <= 262144) summary.final_answer = answer;
  }
  if (complete) { summary.input_tokens = input; summary.output_tokens = output; }
  if (complete && cacheKnown) summary.cached_tokens = cached;
  mkdirSync(dirname(destination), { recursive: true, mode: 0o700 });
  const temporary = `${destination}.${process.pid}.tmp`;
  writeFileSync(temporary, JSON.stringify(summary), { mode: 0o600 });
  renameSync(temporary, destination);
}

export default function(pi) {
  pi.on("turn_end", (event) => {
    const message = event?.message;
    if (message?.role !== "assistant") return;
    steps += 1;
    const usage = event?.usage ?? message?.usage;
    const inTokens = number(usage?.input);
    const outTokens = number(usage?.output);
    if (inTokens === undefined || outTokens === undefined) { usageComplete = false; return; }
    input += inTokens;
    output += outTokens;
    const cacheRead = number(usage?.cacheRead);
    const cacheWrite = number(usage?.cacheWrite);
    if (cacheRead === undefined || cacheWrite === undefined) cacheKnown = false;
    else { input += cacheRead + cacheWrite; cached += cacheRead; }
  });
  pi.on("agent_end", (event) => publish(event));
}
"#;

/// Pi's extension API can register tools but cannot substitute a result for a
/// built-in tool after it has run.  Tape-backed executions therefore disable
/// every built-in tool and load a run-private extension which calls the ASB
/// sealed local-tool service instead.
fn prepare_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    let launch =
        super::super::tool_replay_mcp::per_call_launch(context, tape, TOOL_PROGRESS_FILENAME)?;
    let extension = context.runtime_dir.join("pi/asb-local-tools.ts");
    let parent = extension.parent().expect("Pi extension has a parent");
    std::fs::create_dir_all(parent).map_err(|error| {
        Error::Run(format!(
            "cannot create Pi local-tool extension directory: {error}"
        ))
    })?;
    std::fs::write(&extension, TOOL_REPLAY_EXTENSION)
        .map_err(|error| Error::Run(format!("cannot write Pi local-tool extension: {error}")))?;
    // `OsString` has a platform-specific structured serde representation.
    // Pi's Node extension needs a JSON command line, so render the already
    // validated private paths/arguments as plain strings before serializing.
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
    let launch = serde_json::json!({
        "program": program,
        "args": args,
    });
    context.command.args([
        "--no-builtin-tools",
        "--no-extensions",
        "--extension",
        extension
            .to_str()
            .ok_or_else(|| Error::Run("Pi extension path is not valid UTF-8".into()))?,
    ]);
    context.command.env(
        "ASB_PI_LOCAL_TOOL_REPLAY",
        serde_json::to_string(&launch)
            .map_err(|error| Error::Run(format!("cannot render Pi tool launch: {error}")))?,
    );
    Ok(())
}

// Kept independent of the agent prompt: the selected model receives these
// schemas from Pi, and every execution is routed through the same confined
// record/replay service.  The extension intentionally accepts argv rather
// than a shell program, so it cannot introduce an unrecorded shell parser.
const TOOL_REPLAY_EXTENSION: &str = r#"
import { Type } from "typebox";
import { spawnSync } from "node:child_process";

const spec = JSON.parse(process.env.ASB_PI_LOCAL_TOOL_REPLAY ?? "");
if (!spec.program || !Array.isArray(spec.args)) throw new Error("ASB local tool service is unavailable");
function call(name, args) {
  // Pi 0.73 can leave an asynchronously spawned extension child unread after
  // the first tool result in the isolated guest runtime. Execute one sealed
  // MCP request per tool invocation instead: stdin reaches EOF, the service
  // atomically publishes its transcript, and any launcher/stdio failure is
  // returned to Pi rather than leaving the agent turn pending indefinitely.
  const request = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name, arguments: args } }) + "\n";
  const child = spawnSync(spec.program, spec.args, {
    input: request,
    encoding: "utf8",
    maxBuffer: 1024 * 1024,
    timeout: 45000,
  });
  const stderr = String(child.stderr ?? "").trim();
  if (child.error) throw new Error(`ASB local tool service failed to start: ${child.error.message}`);
  if (child.signal || child.status !== 0) {
    const status = child.signal ? `stopped by ${child.signal}` : `exited with status ${child.status ?? "unknown"}`;
    throw new Error(`ASB local tool service ${status}${stderr ? `: ${stderr}` : ""}`);
  }
  const response = String(child.stdout ?? "").trim().split(/\r?\n/).map((line) => JSON.parse(line)).find((value) => value.id === 1);
  if (!response) throw new Error(`ASB local tool service returned no response${stderr ? `: ${stderr}` : ""}`);
  if (response.error) throw new Error("ASB local tool request failed");
  return JSON.parse(response.result?.content?.[0]?.text ?? "{}");
}
function result(value) { return { content: [{ type: "text", text: JSON.stringify(value) }], details: {} }; }
function path(value) { return typeof value === "string" ? value.replace(/^@/, "") : value; }
function tool(name, label, description, parameters, handler) {
  return { name, label, description, parameters, executionMode: "sequential", async execute(_id, params) { return result(await handler(params)); } };
}
export default function(pi) {
  pi.registerTool(tool("read", "Read", "Read a UTF-8 workspace file.", Type.Object({ path: Type.String() }), (p) => call("read_file", { path: path(p.path) })));
  pi.registerTool(tool("write", "Write", "Create or replace a UTF-8 workspace file.", Type.Object({ path: Type.String(), content: Type.String() }), (p) => call("write_file", { path: path(p.path), content: p.content })));
  pi.registerTool(tool("edit", "Edit", "Replace exactly one matching text fragment in a workspace file.", Type.Object({ path: Type.String(), oldText: Type.String(), newText: Type.String() }), async (p) => {
    const file = path(p.path); const current = await call("read_file", { path: file });
    if (typeof current.text !== "string" || !current.text.includes(p.oldText)) throw new Error("edit target text was not found");
    return call("write_file", { path: file, content: current.text.replace(p.oldText, p.newText) });
  }));
  pi.registerTool(tool("bash", "Command", "Run an allowlisted workspace command. Supply argv, never a shell string.", Type.Object({ argv: Type.Array(Type.String(), { minItems: 1, maxItems: 32 }) }), (p) => call("run_command", { argv: p.argv })));
  pi.registerTool(tool("grep", "Search", "Search bounded UTF-8 workspace text.", Type.Object({ query: Type.String(), path: Type.Optional(Type.String()) }), (p) => call("search_text", { query: p.query, ...(p.path ? { path: path(p.path) } : {}) })));
  pi.registerTool(tool("ls", "List", "List direct non-symlink workspace entries.", Type.Object({ path: Type.Optional(Type.String()) }), (p) => call("list_directory", p.path ? { path: path(p.path) } : {})));
  pi.registerTool(tool("find", "Find", "List direct non-symlink workspace entries at a path.", Type.Object({ path: Type.Optional(Type.String()) }), (p) => call("list_directory", p.path ? { path: path(p.path) } : {})));
}
"#;

fn configure_provider(
    command: &mut std::process::Command,
    directory: &Path,
    provider: &str,
    model: &str,
    base_url: &str,
    api: &str,
    api_key: &str,
) -> Result<(), Error> {
    std::fs::create_dir_all(directory).map_err(|error| {
        Error::Run(format!(
            "cannot create Pi provider configuration directory: {error}"
        ))
    })?;
    let configuration = serde_json::json!({
        "providers": { provider: {
            "baseUrl": base_url, "api": api, "apiKey": api_key,
            "models": [{
                "id": model, "name": model, "reasoning": true, "input": ["text"],
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                "contextWindow": 128000, "maxTokens": 16384
            }]
        }}
    });
    let path = directory.join("models.json");
    let content = serde_json::to_vec_pretty(&configuration)
        .map_err(|error| Error::Run(format!("cannot render Pi provider configuration: {error}")))?;
    std::fs::write(&path, content)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", path.display())))?;
    command.env("PI_CODING_AGENT_DIR", directory);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentAdapter;

    fn frozen_collection_artifacts(
        progress: serde_json::Value,
        transcript: Vec<u8>,
    ) -> crate::agents::FrozenArtifacts {
        let envelope = serde_json::json!({
            "schema_version": 1,
            "documents": {"pi-progress.json": progress},
            "bytes": {"local-tools.jsonl": transcript},
        });
        crate::agents::FrozenArtifacts::from_deferred_collection_bytes(
            &serde_json::to_vec(&envelope).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn deferred_collection_uses_the_exported_per_call_mcp_receipt() {
        let root = std::env::temp_dir().join(format!(
            "asb-pi-deferred-receipt-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let frozen = frozen_collection_artifacts(
            serde_json::json!({"schema_version": 1, "cursor": 0, "events": 0}),
            Vec::new(),
        );
        let artifacts = collect(&CollectionContext {
            frozen: &frozen,
            provider_capture: None,
            run_dir: &root,
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
        });
        assert!(artifacts.errors.is_empty(), "{:?}", artifacts.errors);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn deferred_collection_rejects_an_unconsumed_per_call_mcp_receipt() {
        let root = std::env::temp_dir().join(format!(
            "asb-pi-deferred-receipt-reject-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let frozen = frozen_collection_artifacts(
            serde_json::json!({"schema_version": 1, "cursor": 0, "events": 1}),
            Vec::new(),
        );
        let artifacts = collect(&CollectionContext {
            frozen: &frozen,
            provider_capture: None,
            run_dir: &root,
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
        });
        assert_eq!(
            artifacts.errors,
            ["local-tool replay left recorded events unconsumed"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn authentication_diagnostic_is_scoped_to_pi_stderr_and_terminal_state() {
        let diagnostic = "Authentication required. Please use /login command to sign in to your account fixture-secret";
        let reason = ADAPTER
            .execution_failure("", diagnostic)
            .expect("Pi diagnostic");
        assert!(reason.contains("authentication failure"));
        assert!(!reason.contains("fixture-secret"));
        assert!(ADAPTER.execution_failure(diagnostic, "").is_none());
        assert!(
            ADAPTER
                .execution_failure(
                    r#"{"type":"turn_end","message":{"stopReason":"stop"}}"#,
                    diagnostic
                )
                .is_some()
        );
    }

    #[test]
    fn text_answer_cannot_spoof_step_count() {
        let answer = concat!(
            "{\"type\":\"agent_start\"}\n",
            "{\"type\":\"turn_start\"}\n",
            "{\"type\":\"turn_end\",\"message\":{\"role\":\"assistant\"}}\n",
            "{\"type\":\"agent_end\"}\n",
        );
        assert_eq!(ADAPTER.step_count(answer, ""), None);
    }

    #[test]
    fn provider_configuration_is_owned_by_the_pi_adapter() {
        let directory = std::env::temp_dir().join(format!("asb-pi-config-{}", std::process::id()));
        let mut command = std::process::Command::new("pi");
        configure_provider(
            &mut command,
            &directory,
            "cli2key",
            "test-model",
            "http://127.0.0.1:3456/v1",
            "openai-responses",
            "CLI2KEY_API_KEY",
        )
        .expect("configuration succeeds");
        let text =
            std::fs::read_to_string(directory.join("models.json")).expect("configuration exists");
        assert!(text.contains("test-model"));
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn generic_deterministic_extension_is_initialized_once() {
        let root = std::env::temp_dir().join(format!(
            "asb-pi-determinism-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        let run_dir = root.join("run");
        std::fs::create_dir_all(&workspace).unwrap();
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiResponses,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("fixture-key".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let mut command = std::process::Command::new("pi");
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
                tape: None,
            })
            .unwrap();
        let trace = super::super::super::native_trace::Transcript::decode(
            &std::fs::read(run_dir.join("native-trace/native-trace.jsonl")).unwrap(),
        )
        .unwrap();
        assert_eq!(trace.len(), 1);
        assert_eq!(
            trace.events()[0].operation,
            super::super::super::native_trace::Operation::Host
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tape_backed_preparation_replaces_pi_builtin_tools_with_private_extension() {
        let root = std::env::temp_dir().join(format!(
            "asb-pi-tool-replay-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        let run_dir = root.join("run");
        std::fs::create_dir_all(&workspace).unwrap();
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiResponses,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("asb-replay".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let mut command = std::process::Command::new("pi");
        prepare_tool_replay(
            &mut PreparationContext {
                command: &mut command,
                model: "test-model",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &root.join("runtime"),
                run_dir: &run_dir,
                mcp_servers: &[],
                mcp_tape: None,
                tape: None,
            },
            super::super::TapeContext {
                mode: crate::tape::Mode::Record,
                cassette: &root.join("recording.asb.zip"),
            },
        )
        .unwrap();
        let arguments = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(arguments.contains(&"--no-builtin-tools".into()));
        assert!(arguments.contains(&"--no-extensions".into()));
        let extension = root.join("runtime/pi/asb-local-tools.ts");
        let source = std::fs::read_to_string(extension).unwrap();
        assert!(source.contains("run_command"));
        assert!(source.contains("spawnSync(spec.program, spec.args"));
        assert!(source.contains("timeout: 45000"));
        assert!(source.contains("returned no response"));
        // The generated TypeScript must send a real line delimiter, not the
        // two literal bytes backslash+n that make the Rust JSONL server see
        // one malformed request.
        assert!(source.contains(r#"}) + "\n";"#));
        assert!(!source.contains(r#"}) + "\\n";"#));
        assert!(!source.contains("const pending = new Map()"));
        assert!(run_dir.join("tool-replay/local-tools.jsonl").is_file());
        assert!(run_dir.join("tool-replay/pi-progress.json").is_file());
        let launch = command
            .get_envs()
            .find(|(name, _)| name.to_string_lossy() == "ASB_PI_LOCAL_TOOL_REPLAY")
            .and_then(|(_, value)| value)
            .expect("Pi local-tool launch specification");
        let launch = serde_json::from_str::<serde_json::Value>(&launch.to_string_lossy()).unwrap();
        assert_eq!(launch["args"][0], "__asb-tool-replay");
        assert!(
            launch["args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| { value == "--per-call-state" })
        );
        assert!(
            launch["args"]
                .as_array()
                .unwrap()
                .iter()
                .all(|value| value.is_string())
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
