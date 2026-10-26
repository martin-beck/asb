use super::{CommandContext, CommandSpec, PreparationContext, PromptDelivery, StaticAdapter};
use crate::{Error, install::Recipe};
use std::path::Path;
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
.with_tool_replay(prepare_tool_replay)
.with_artifact_specs(&[
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "local-tools.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "determinism",
        filename: "manifest.json",
        format: super::super::ArtifactFormat::Json,
    },
])
.with_deterministic_extension()
.with_output_usage(output_usage)
.with_state_environment_vars(&["PI_CODING_AGENT_DIR"])
.with_execution_failure(execution_failure)
.with_step_count(step_count);

fn execution_failure(stdout: &str, _: &str) -> Option<String> {
    let mut failed = false;
    for event in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        let message = if event["type"] == "turn_end" {
            event.get("message")
        } else if event["type"] == "agent_end" {
            event
                .get("messages")
                .and_then(|messages| messages.as_array())
                .and_then(|messages| {
                    messages
                        .iter()
                        .rev()
                        .find(|message| message["role"] == "assistant")
                })
        } else {
            None
        };
        if let Some(reason) = message
            .and_then(|message| message.get("stopReason"))
            .and_then(|reason| reason.as_str())
        {
            failed = matches!(reason, "error" | "aborted");
        }
    }
    failed.then(|| "agent reported a terminal API request failure or cancellation; inspect the private agent log".into())
}

#[cfg(test)]
mod execution_tests {
    use super::execution_failure;

    #[test]
    fn terminal_api_errors_are_failures_without_echoing_provider_bodies() {
        let output = serde_json::json!({"type":"agent_end","messages":[{"role":"assistant",
            "stopReason":"error","errorMessage":"400 private-provider-body"}]})
        .to_string();
        let failure = execution_failure(&output, "").unwrap();
        assert!(failure.contains("API request"));
        assert!(!failure.contains("private-provider-body"));
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
}

fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    let (mut active, mut turn_open, mut completed) = (false, false, false);
    let (mut retry_pending, mut compaction_pending) = (false, false);
    let mut continuation_pending = false;
    let mut turns = 0_u64;
    let mut cycle_turns = 0_u64;
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let event: serde_json::Value = serde_json::from_str(line).ok()?;
        match event.get("type")?.as_str()? {
            "agent_start" if !active => {
                active = true;
                completed = false;
                continuation_pending = false;
                cycle_turns = 0;
            }
            "turn_start" if active && !turn_open => turn_open = true,
            "turn_end"
                if active
                    && turn_open
                    && event.pointer("/message/role").and_then(|v| v.as_str())
                        == Some("assistant") =>
            {
                // One terminal event per attempted main-agent model turn.
                // Error/aborted turns count too; agent_end repeats messages.
                turns = turns.checked_add(1)?;
                cycle_turns = cycle_turns.checked_add(1)?;
                turn_open = false;
            }
            "agent_end" if active && !turn_open && cycle_turns > 0 => {
                active = false;
                completed = true;
            }
            "agent_start" | "turn_start" | "turn_end" | "agent_end" => return None,
            "auto_retry_start" => retry_pending = true,
            "auto_retry_end" => retry_pending = false,
            // Retry starts may repeat across failed attempts, and terminal
            // retry/compaction errors can end without a start in Pi itself.
            "compaction_start" => compaction_pending = true,
            "compaction_end" => {
                compaction_pending = false;
                continuation_pending = event.get("willRetry")?.as_bool()?;
            }
            _ => {}
        }
    }
    (completed
        && !active
        && !turn_open
        && !retry_pending
        && !compaction_pending
        && !continuation_pending
        && turns > 0)
        .then_some(turns)
}

fn output_usage(stdout: &str, stderr: &str, _: bool) -> crate::measurements::TokenUsage {
    let mut sum = crate::measurements::TokenUsage::default();
    let mut seen = false;
    let mut cache_complete = true;
    for event in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if event.get("type").and_then(|v| v.as_str()) != Some("turn_end") {
            continue;
        }
        let Some(value) = event
            .get("usage")
            .or_else(|| event.pointer("/message/usage"))
        else {
            return Default::default();
        };
        let number = |key: &str| value.get(key).and_then(|v| v.as_u64());
        let (Some(input), Some(output)) = (number("input"), number("output")) else {
            return Default::default();
        };
        let read = number("cacheRead");
        cache_complete &= read.is_some();
        sum = sum.add(crate::measurements::TokenUsage {
            input_tokens: Some(
                input
                    .saturating_add(read.unwrap_or(0))
                    .saturating_add(number("cacheWrite").unwrap_or(0)),
            ),
            output_tokens: Some(output),
            cached_tokens: read,
            ..Default::default()
        });
        seen = true;
    }
    if seen {
        if !cache_complete {
            sum.cached_tokens = None;
        }
        sum.with_derived_total()
    } else {
        crate::usage::usage_from_text(&format!("{stdout}\n{stderr}"))
    }
}
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "pi".into(),
        args: vec![
            "--provider".into(),
            "asb".into(),
            "--model".into(),
            context.config.model.clone(),
            "--mode".into(),
            "json".into(),
        ],
        prompt_delivery: PromptDelivery::Argument,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    super::super::determinism::prepare("pi", context)?;
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
    )
}

/// Pi's extension API can register tools but cannot substitute a result for a
/// built-in tool after it has run.  Tape-backed executions therefore disable
/// every built-in tool and load a run-private extension which calls the ASB
/// sealed local-tool service instead.
fn prepare_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    let launch = super::super::tool_replay_mcp::launch(context, tape)?;
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
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

const spec = JSON.parse(process.env.ASB_PI_LOCAL_TOOL_REPLAY ?? "");
if (!spec.program || !Array.isArray(spec.args)) throw new Error("ASB local tool service is unavailable");
const child = spawn(spec.program, spec.args, { stdio: ["pipe", "pipe", "pipe"] });
let nextId = 1;
const pending = new Map();
let terminalError = "";
child.stderr.setEncoding("utf8");
child.stderr.on("data", (chunk) => { terminalError = (terminalError + chunk).slice(-2048); });
function serviceError(detail) {
  const suffix = terminalError.trim();
  return new Error(`ASB local tool service ${detail}${suffix ? `: ${suffix}` : ""}`);
}
createInterface({ input: child.stdout }).on("line", (line) => {
  let response;
  try { response = JSON.parse(line); } catch { return; }
  const entry = pending.get(response.id);
  if (!entry) return;
  pending.delete(response.id);
  if (response.error) entry.reject(new Error("ASB local tool request failed"));
  else entry.resolve(response.result);
});
child.once("error", () => {
  for (const entry of pending.values()) entry.reject(serviceError("failed to start"));
  pending.clear();
});
child.once("exit", (code, signal) => {
  if (pending.size === 0) return;
  const status = signal ? `stopped by ${signal}` : `exited with status ${code ?? "unknown"}`;
  for (const entry of pending.values()) entry.reject(serviceError(status));
  pending.clear();
});
function call(name, args) {
  return new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method: "tools/call", params: { name, arguments: args } }) + "\\n", (error) => {
      if (!error || !pending.has(id)) return;
      pending.delete(id);
      reject(serviceError("could not receive a request"));
    });
  }).then((result) => JSON.parse(result.content?.[0]?.text ?? "{}"));
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

    #[test]
    fn counts_only_balanced_terminal_turns_in_complete_agent_cycles() {
        let cycle = r#"{"type":"agent_start"}
{"type":"turn_start"}
{"type":"message_end","message":{"role":"assistant"}}
{"type":"tool_execution_end"}
{"type":"turn_end","message":{"role":"assistant","stopReason":"error"}}
{"type":"agent_end","messages":[{"role":"assistant"},{"role":"assistant"}]}"#;
        assert_eq!(step_count(cycle, ""), Some(1));
        let retry = format!(
            "{cycle}\n{{\"type\":\"auto_retry_start\"}}\n{cycle}\n{{\"type\":\"auto_retry_end\"}}"
        );
        assert_eq!(step_count(&retry, ""), Some(2));
        assert_eq!(
            step_count(&format!("{cycle}\n{{\"type\":\"auto_retry_start\"}}"), ""),
            None
        );
        assert_eq!(
            step_count(&format!("{cycle}\n{{\"type\":\"compaction_start\"}}"), ""),
            None
        );
        assert_eq!(
            step_count(&format!("{cycle}\n{{\"type\":\"turn_end\"}}"), ""),
            None
        );
        assert_eq!(step_count(&format!("{cycle}\nbroken"), ""), None);
        assert_eq!(step_count(cycle.rsplit_once('\n').unwrap().0, ""), None);
        assert_eq!(
            step_count(r#"{"type":"agent_end","messages":[]}"#, ""),
            None
        );
        assert_eq!(step_count("", ""), None);
        assert_eq!(
            step_count(
                &format!("{cycle}\n{{\"type\":\"agent_start\"}}\n{{\"type\":\"agent_end\"}}"),
                ""
            ),
            None
        );
        let compaction = format!(
            "{cycle}\n{{\"type\":\"compaction_start\"}}\n{{\"type\":\"compaction_end\",\"willRetry\":true}}"
        );
        assert_eq!(step_count(&compaction, ""), None);
        assert_eq!(step_count(&format!("{compaction}\n{cycle}"), ""), Some(2));
        assert_eq!(
            step_count(
                &format!("{cycle}\n{{\"type\":\"compaction_end\",\"willRetry\":false}}"),
                ""
            ),
            Some(1)
        );
        assert_eq!(
            step_count(
                &format!(
                    "{cycle}\n{{\"type\":\"auto_retry_end\",\"success\":false,\"attempt\":0}}"
                ),
                ""
            ),
            Some(1)
        );
        let repeated_retry = format!(
            "{cycle}\n{{\"type\":\"auto_retry_start\",\"attempt\":1}}\n{cycle}\n{{\"type\":\"auto_retry_start\",\"attempt\":2}}\n{cycle}\n{{\"type\":\"auto_retry_end\",\"success\":false,\"attempt\":2}}"
        );
        assert_eq!(step_count(&repeated_retry, ""), Some(3));
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
        assert!(source.contains("stdio: [\"pipe\", \"pipe\", \"pipe\"]"));
        assert!(source.contains("child.once(\"exit\""));
        assert!(source.contains("terminalError"));
        assert!(run_dir.join("tool-replay/local-tools.jsonl").is_file());
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
                .all(|value| value.is_string())
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
