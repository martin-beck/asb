use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, PreparationContext,
    PromptDelivery, StaticAdapter,
};
use crate::{Error, install::Recipe};

// OpenCode's Bedrock integration currently resolves two AWS SDK entries whose
// published registry metadata can outpace their tarballs. This repair belongs
// to the owning adapter, never to unrelated npm-installed tools or agents.
const NPM_MANIFEST: &str = r#"{
  "private": true,
  "overrides": {
    "@aws-sdk/client-bedrock-runtime": "3.1030.0",
    "@aws-sdk/credential-provider-http": "3.972.76"
  }
}
"#;

// OpenCode applies its MCP timeout to tool calls as well as startup. Match
// ASB's largest accepted agent execution budget so the owning process deadline,
// rather than a shorter transport timeout, controls a running tool call.
const LOCAL_TOOL_TIMEOUT_MS: u64 = 86_400_000;
// OpenCode regenerates these private output and delegated-task identifiers on
// replay; the actual tool output and task state remain comparison-significant.
const TAPE_WIRE_EXTENSION: &str = r#"
export function normalizeRequest(request) {
  const visit = (value) => {
    if (typeof value === 'string') return value
      .replace(/(<task id=")ses_[A-Za-z0-9_-]+(" state="completed">)/g, '$1<asb-opencode-task-session>$2')
      .replace(/\/opt\/asb-private\/agent-state\/data\/opencode\/tool-output\/tool_[A-Za-z0-9_-]+/g, '<asb-opencode-tool-output>');
    if (Array.isArray(value)) return value.map(visit);
    if (value && typeof value === 'object') for (const key of Object.keys(value)) value[key] = visit(value[key]);
    return value;
  };
  return visit(request?.body);
}
"#;

pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "opencode",
    "opencode",
    Recipe::Npm {
        package: "opencode-ai@1.19.0",
        executable: "opencode",
        manifest: Some(NPM_MANIFEST),
    },
    &[
        crate::endpoint::CHAT,
        crate::endpoint::MESSAGES_KEY,
        crate::endpoint::RESPONSES,
    ],
    command,
    prepare_adapter,
)
.with_estimated_disk_mib(1024)
.with_tape_wire_extension(TAPE_WIRE_EXTENSION)
.with_tool_replay(prepare_tool_replay)
.with_native_tool_transport()
.with_guest_replay_evidence(extract_guest_replay_evidence)
.with_guest_service(start_native_service)
.with_snapshot_replay_settlement()
.with_deferred_native_recording(finalize_native_recording)
.with_native_snapshot_check(verify_native_snapshot)
.with_artifact_specs(&[
    super::super::ArtifactSpec {
        namespace: "opencode",
        filename: "native-tool-pending.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "local-tools.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "tool-protocol.json",
        format: super::super::ArtifactFormat::Json,
    },
])
.with_collection(collect)
.with_output_usage(output_usage)
.with_final_response(final_response)
.with_step_count(step_count)
.with_tool_call_count(tool_call_count)
.with_state_environment_vars(&[
    "OPENCODE_CONFIG_CONTENT",
    "OPENCODE_CONFIG",
    "OPENCODE_CONFIG_DIR",
]);

/// OpenCode emits one JSONL `step_finish` event for every completed agent step.
/// A terminal answer may be a plain text event, so it must not be used as a
/// substitute for this lifecycle counter.
fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    let steps = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event.get("type").and_then(|value| value.as_str()) == Some("step_finish"))
        .count()
        .try_into()
        .ok()?;
    (steps > 0).then_some(steps)
}

/// Count distinct finished native tool calls in a complete OpenCode JSON stream.
fn tool_call_count(stdout: &str, _stderr: &str) -> Option<usize> {
    let mut calls = std::collections::HashSet::new();
    let mut finished = false;
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let event: serde_json::Value = serde_json::from_str(line).ok()?;
        match event.get("type").and_then(|value| value.as_str())? {
            "tool_use" => {
                let part = event.get("part")?;
                let status = part.get("state")?.get("status")?.as_str()?;
                if !matches!(status, "completed" | "error") {
                    return None;
                }
                let session = part.get("sessionID")?.as_str()?;
                let call = part.get("callID")?.as_str()?;
                if session.is_empty() || call.is_empty() {
                    return None;
                }
                calls.insert((session.to_owned(), call.to_owned()));
            }
            "step_finish" => {
                if event.get("part")?.get("reason")?.as_str()? == "stop" {
                    finished = true;
                }
            }
            _ => {}
        }
    }
    finished.then_some(calls.len())
}

#[cfg(test)]
mod native_tool_count_tests {
    use super::tool_call_count;

    #[test]
    fn counts_finished_distinct_calls_only_with_terminal_step() {
        let tool = r#"{"type":"tool_use","part":{"sessionID":"s","callID":"c","state":{"status":"completed"}}}"#;
        let failed = r#"{"type":"tool_use","part":{"sessionID":"s","callID":"d","state":{"status":"error"}}}"#;
        let stop = r#"{"type":"step_finish","part":{"reason":"stop"}}"#;
        assert_eq!(
            tool_call_count(&format!("{tool}\n{tool}\n{failed}\n{stop}"), ""),
            Some(2)
        );
        assert_eq!(tool_call_count(stop, ""), Some(0));
        assert_eq!(tool_call_count(tool, ""), None);
        assert_eq!(tool_call_count(&format!("{tool}\n{{\n{stop}"), ""), None);
        assert_eq!(
            tool_call_count(
                &format!("{}\n{stop}", tool.replace("completed", "running")),
                ""
            ),
            None
        );
    }
}

fn final_response(output: &str, _: &std::path::Path) -> Option<String> {
    let mut texts =
        std::collections::HashMap::<(String, String), Vec<(Option<String>, String)>>::new();
    let mut final_answer = None;
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        // A truncated or malformed native stream cannot prove completion.
        let event = serde_json::from_str::<serde_json::Value>(line).ok()?;
        let kind = event.get("type").and_then(|v| v.as_str());
        let part = event.get("part");
        let event_session = event.get("sessionID").and_then(|v| v.as_str());
        let part_session = part
            .and_then(|p| p.get("sessionID"))
            .and_then(|v| v.as_str());
        if event_session.zip(part_session).is_some_and(|(a, b)| a != b) {
            return None;
        }
        let session = event_session.or(part_session).unwrap_or_default();
        match kind {
            Some("text") => {
                if let (Some(id), Some(text)) = (
                    part.and_then(|p| p.get("messageID"))
                        .and_then(|v| v.as_str()),
                    part.and_then(|p| p.get("text")).and_then(|v| v.as_str()),
                ) && !text.trim().is_empty()
                {
                    let parts = texts
                        .entry((session.to_owned(), id.to_owned()))
                        .or_default();
                    let part_id = part
                        .and_then(|p| p.get("id"))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);
                    if let Some(existing) = part_id
                        .as_ref()
                        .and_then(|id| parts.iter_mut().find(|(key, _)| key.as_ref() == Some(id)))
                    {
                        existing.1 = text.to_owned();
                    } else {
                        parts.push((part_id, text.to_owned()));
                    }
                }
                final_answer = None;
            }
            Some("step_finish") => {
                final_answer = match (
                    part.and_then(|p| p.get("messageID"))
                        .and_then(|v| v.as_str()),
                    part.and_then(|p| p.get("reason")).and_then(|v| v.as_str()),
                ) {
                    (Some(id), Some(reason))
                        if !reason.is_empty()
                            && !matches!(reason, "tool-calls" | "unknown" | "content-filter") =>
                    {
                        texts
                            .get(&(session.to_owned(), id.to_owned()))
                            .map(|parts| {
                                parts
                                    .iter()
                                    .map(|(_, text)| text.as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                    }
                    _ => None,
                };
                texts.clear();
            }
            Some("step_start" | "tool_use" | "error") => {
                texts.clear();
                final_answer = None;
            }
            _ => {}
        }
    }
    final_answer
}

fn output_usage(stdout: &str, _stderr: &str, _: bool) -> crate::measurements::TokenUsage {
    let mut sum = crate::measurements::TokenUsage::default();
    let mut seen = false;
    let mut native_stream = false;
    let mut cache_complete = true;
    for event in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        let event_type = event.get("type").and_then(|v| v.as_str());
        native_stream |= matches!(
            event_type,
            Some("tool_use" | "step_start" | "step_finish" | "text" | "reasoning" | "error")
        );
        if event_type != Some("step_finish") {
            continue;
        }
        let Some(tokens) = event.pointer("/part/tokens") else {
            return Default::default();
        };
        let number = |name: &str| tokens.get(name).and_then(|v| v.as_u64());
        let (Some(input), Some(output)) = (number("input"), number("output")) else {
            return Default::default();
        };
        let read = tokens.pointer("/cache/read").and_then(|v| v.as_u64());
        let write = tokens.pointer("/cache/write").and_then(|v| v.as_u64());
        cache_complete &= read.is_some();
        sum = sum.add(crate::measurements::TokenUsage {
            input_tokens: Some(
                input
                    .saturating_add(read.unwrap_or(0))
                    .saturating_add(write.unwrap_or(0)),
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
    } else if native_stream {
        // An incomplete native JSON stream has no authoritative completed
        // usage; tool input and task text may themselves contain token data.
        Default::default()
    } else {
        Default::default()
    }
}

fn command(context: &CommandContext<'_>) -> CommandSpec {
    let model = format!("asb/{}", context.config.model);
    CommandSpec {
        program: "opencode".into(),
        args: vec![
            "run".into(),
            "--dir".into(),
            context.config.workspace.clone(),
            "--model".into(),
            model,
            "--format".into(),
            "json".into(),
        ],
        prompt_delivery: PromptDelivery::Argument,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let npm = match connection.interface {
        crate::endpoint::Interface::AnthropicMessages => "@ai-sdk/anthropic",
        crate::endpoint::Interface::OpenAiChat => "@ai-sdk/openai-compatible",
        crate::endpoint::Interface::OpenAiResponses => "@ai-sdk/openai",
        _ => return Err(context.unsupported_endpoint("opencode")),
    };
    // The tool hook runs before the adapter hook.  Consume its private launch
    // description before mutating the command, then remove the bridge
    // variable so it is never exposed to OpenCode or the benchmark process.
    let tool_command = tool_replay(context)?;
    let mut configuration: serde_json::Value = serde_json::from_str(&config(
        context.model,
        &connection.base_url,
        npm,
        tool_command,
    )?)
    .map_err(|_| Error::Run("cannot configure OpenCode provider".into()))?;
    if native_snapshot(context.tape.map(|tape| tape.mode)) {
        context.command.arg("--auto");
        let plugin = context
            .run_dir
            .join("opencode/opencode-native-launcher.mjs");
        configuration["plugin"] = serde_json::json!([plugin]);
        context.command.env(
            "ASB_OPENCODE_EVIDENCE",
            "/opt/asb-private/adapter-model-evidence.json",
        );
    }
    context
        .command
        .env_remove("ASB_OPENCODE_TOOL_REPLAY_COMMAND");
    context
        .command
        .env(
            "ASB_AGENT_API_KEY",
            connection.credential.as_deref().unwrap_or("asb-local"),
        )
        .env("OPENCODE_CONFIG_CONTENT", configuration.to_string());
    // OpenCode's repository-search tool invokes ripgrep.  Fresh workload
    // materialization can give the same files a different directory-entry
    // order, which otherwise changes only the ordering of an identical tool
    // result and breaks strict replay history matching.  Keep this policy in
    // the owning adapter and use a private runtime file, never a host-wide
    // ripgrep configuration.
    std::fs::create_dir_all(context.runtime_dir).map_err(|error| {
        Error::Run(format!(
            "cannot create deterministic OpenCode runtime directory: {error}"
        ))
    })?;
    let ripgrep_config = context.runtime_dir.join("opencode-ripgrep.conf");
    std::fs::write(&ripgrep_config, "--sort=path\n").map_err(|error| {
        Error::Run(format!(
            "cannot write deterministic OpenCode search configuration: {error}"
        ))
    })?;
    context.command.env("RIPGREP_CONFIG_PATH", ripgrep_config);
    Ok(())
}

fn prepare_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    if native_snapshot(Some(tape.mode)) {
        return Ok(());
    }
    super::super::tool_replay_mcp::prepare_protocol_artifact(context, tape)?;
    let launch = super::super::tool_replay_mcp::launch(context, tape)?;
    let command = std::iter::once(launch.program)
        .chain(launch.args.into_iter().map(std::path::PathBuf::from))
        .map(|value| {
            value
                .into_os_string()
                .into_string()
                .map_err(|_| Error::Run("ASB local-tool replay argument is not valid UTF-8".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    context.command.env(
        "ASB_OPENCODE_TOOL_REPLAY_COMMAND",
        serde_json::to_string(&command)
            .map_err(|_| Error::Run("cannot encode OpenCode local-tool command".into()))?,
    );
    Ok(())
}

fn native_snapshot(mode: Option<crate::tape::Mode>) -> bool {
    match mode {
        Some(crate::tape::Mode::Record) => std::env::var_os("ASB_FS_MARKER_SOCKET").is_some(),
        Some(crate::tape::Mode::Replay) => std::env::var_os("ASB_FS_REPLAY_SOCKET").is_some(),
        None => false,
    }
}

fn extract_guest_replay_evidence(track: &std::path::Path) -> Result<Vec<u8>, Error> {
    use std::io::{BufRead, BufReader};
    let metadata = std::fs::symlink_metadata(track)
        .map_err(|_| Error::Run("OpenCode replay model track is unavailable".into()))?;
    if !metadata.file_type().is_file()
        || metadata.len() > crate::cassette::Limits::default().max_track_bytes
    {
        return Err(Error::Run(
            "OpenCode replay model track is invalid or too large".into(),
        ));
    }
    let file = std::fs::File::open(track).map_err(crate::io_error)?;
    let mut definitions = serde_json::Map::new();
    for line in BufReader::new(file).lines() {
        let line = line.map_err(crate::io_error)?;
        let row: serde_json::Value = serde_json::from_str(&line)
            .map_err(|_| Error::Run("OpenCode replay model track is malformed".into()))?;
        let Some(raw) = row.pointer("/request/body").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(body) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        let Some(tools) = body.get("tools").and_then(|v| v.as_array()) else {
            continue;
        };
        for entry in tools {
            let tool = entry.get("function").unwrap_or(entry);
            let (Some(name), Some(schema)) = (
                tool.get("name").and_then(|v| v.as_str()),
                tool.get("parameters").or_else(|| tool.get("input_schema")),
            ) else {
                continue;
            };
            definitions.entry(name.to_owned()).or_insert_with(|| {
                serde_json::json!({
                    "description": tool.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                    "schema": schema,
                })
            });
        }
    }
    serde_json::to_vec(&definitions)
        .map_err(|_| Error::Run("cannot encode OpenCode tool definitions".into()))
}

fn start_native_service(
    command: &mut std::process::Command,
    executable: &str,
    run_dir: &std::path::Path,
    mode: Option<crate::tape::Mode>,
    native_tool: Option<&std::path::Path>,
    snapshot_sha256: Option<&str>,
) -> Result<Option<super::super::auxiliary::AuxiliaryPlan>, Error> {
    let Some((mode, marker, snapshot)) =
        super::super::native_guest::selection(mode, snapshot_sha256)?
    else {
        return Ok(None);
    };
    let bundle = std::fs::canonicalize(executable).map_err(|_| {
        Error::Run("OpenCode executable is unavailable for native recording".into())
    })?;
    let assets = [
        (
            "opencode-native-plugin.mjs",
            include_str!("../../../assets/scripts/opencode-native-plugin.mjs"),
        ),
        (
            "opencode-native-launcher.mjs",
            include_str!("../../../assets/scripts/opencode-native-launcher.mjs"),
        ),
    ];
    let settings = super::super::native_guest::Settings {
        agent: "opencode",
        bundle: &bundle,
        run_dir,
        mode,
        marker: marker.clone(),
        native_tool,
        snapshot: &snapshot,
        assets: &assets,
    };
    let (mut service, directory, socket) = super::super::native_guest::broker(command, &settings)?;
    if mode == crate::tape::Mode::Replay {
        let sidecar = native_tool
            .ok_or_else(|| Error::Run("OpenCode native replay sidecar is missing".into()))?;
        let mut preflight = std::process::Command::new("node");
        if let Some((_, Some(path))) = command.get_envs().find(|(name, _)| *name == "PATH") {
            preflight.env("PATH", path);
        }
        let status = preflight.arg("--input-type=module")
            .arg("-e")
            .arg("import {pathToFileURL} from 'node:url'; const m = await import(pathToFileURL(process.argv[1])); await m.recordedDefinitions(process.argv[2], process.argv[3]);")
            .arg(directory.join("opencode-native-launcher.mjs"))
            .arg("/opt/asb-private/adapter-model-evidence.json")
            .arg(sidecar)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|_| Error::Run("cannot check OpenCode replay tool definitions; verify Node is installed".into()))?;
        if !status.success() {
            return Err(Error::Run("OpenCode replay tool definitions are missing or invalid; use a complete recording for this agent".into()));
        }
    }
    let state = directory.join("hook-state");
    std::fs::create_dir_all(&state).map_err(crate::io_error)?;
    service
        .env(
            "ASB_NATIVE_SETTLEMENT_MODULE",
            directory.join("opencode-native-plugin.mjs"),
        )
        .env("ASB_NATIVE_SETTLEMENT_DIRECTORY", &state)
        .env("ASB_NATIVE_SETTLEMENT_MARKER", &marker);
    command
        .env("ASB_OPENCODE_SOCKET", socket)
        .env("ASB_OPENCODE_STATE", state)
        .env("ASB_OPENCODE_MARKER", marker)
        .env(
            "ASB_OPENCODE_NATIVE_MODE",
            if mode == crate::tape::Mode::Replay {
                "replay"
            } else {
                "record"
            },
        );
    if let Some(path) = native_tool {
        command.env("ASB_OPENCODE_SIDECAR", path);
    }
    Ok(Some(super::super::auxiliary::AuxiliaryPlan::new(service)))
}

fn finalize_native_recording(pending: &[u8], digest: &str) -> Result<Vec<u8>, Error> {
    super::super::native_guest::finalize("opencode", pending, digest)
}

fn verify_native_snapshot(
    native: &[u8],
    snapshot: &mut std::fs::File,
    digest: &str,
) -> Result<(), Error> {
    super::pi::snapshot::verify_for("opencode", native, snapshot, digest)
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let mut artifacts = AgentArtifacts::default();
    if let Some(bytes) = context
        .frozen
        .bytes("native-tool-pending.jsonl")
        .filter(|bytes| !bytes.is_empty())
    {
        artifacts
            .sealed_artifacts
            .push(super::super::SealedArtifact {
                kind: "native-tool-deferred".into(),
                bytes: bytes.to_vec(),
            });
    }
    if let Some(bytes) = context.frozen.bytes("tool-protocol.json") {
        if let Err(error) = super::super::tool_replay_mcp::verify_protocol_bytes(bytes) {
            artifacts.errors.push(error.to_string());
        } else {
            artifacts
                .sealed_artifacts
                .push(super::super::SealedArtifact {
                    kind: "tool-protocol".into(),
                    bytes: bytes.to_vec(),
                });
        }
    }
    artifacts
}

fn tool_replay(context: &PreparationContext<'_>) -> Result<Option<Vec<String>>, Error> {
    let Some(command) = context
        .command
        .get_envs()
        .find_map(|(name, value)| (name == "ASB_OPENCODE_TOOL_REPLAY_COMMAND").then_some(value))
        .flatten()
    else {
        return Ok(None);
    };
    serde_json::from_slice(command.as_encoded_bytes())
        .map(Some)
        .map_err(|_| Error::Run("OpenCode local-tool command is malformed".into()))
}

fn config(
    model: &str,
    base_url: &str,
    npm: &str,
    tool_command: Option<Vec<String>>,
) -> Result<String, Error> {
    let mut models = serde_json::Map::new();
    models.insert(model.to_owned(), serde_json::json!({"name": model}));
    let mut configuration = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {"asb": {
            "npm": npm, "name": "ASB provider",
            "options": {"baseURL": base_url, "apiKey": "{env:ASB_AGENT_API_KEY}"},
            "models": models
        }}
    });
    if let Some(command) = tool_command {
        configuration["mcp"] = serde_json::json!({
            "asb_local_tools": {
                "type":"local",
                "command":command,
                "enabled":true,
                "timeout":LOCAL_TOOL_TIMEOUT_MS
            }
        });
        // The public OpenCode configuration exposes these as every built-in
        // executable/action tool.  Denying them leaves the sealed ASB MCP
        // service as the sole benchmark-visible local-tool authority.
        configuration["permission"] = serde_json::json!({
            "bash":"deny", "edit":"deny", "read":"deny", "grep":"deny",
            "glob":"deny", "lsp":"deny", "apply_patch":"deny", "skill":"deny",
            "todowrite":"deny", "webfetch":"deny", "websearch":"deny", "question":"deny",
            "asb_local_tools_*":"allow"
        });
    }
    serde_json::to_string(&configuration)
        .map_err(|error| Error::Run(format!("cannot configure OpenCode provider: {error}")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn guest_replay_evidence_contains_only_tool_definitions() {
        let root = std::env::temp_dir().join(format!(
            "asb-opencode-definitions-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let track = root.join("track.jsonl");
        let body = serde_json::json!({"messages":[{"content":"private prompt"}],"tools":[{"type":"function","function":{"name":"bash","description":"shell","parameters":{"type":"object"}}}]});
        std::fs::write(
            &track,
            format!(
                "{}\n",
                serde_json::json!({"request":{"body":body.to_string()}})
            ),
        )
        .unwrap();
        let evidence = super::extract_guest_replay_evidence(&track).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&evidence).unwrap();
        assert_eq!(value["bash"]["schema"]["type"], "object");
        assert!(
            !String::from_utf8(evidence)
                .unwrap()
                .contains("private prompt")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn native_pending_recording_is_collected_for_cassette_publication() {
        let frozen = crate::agents::FrozenArtifacts::from_deferred_collection_bytes(
            &serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "documents": {},
                "bytes": {"native-tool-pending.jsonl": [123, 125, 10]}
            }))
            .unwrap(),
        )
        .unwrap();
        let artifacts = super::collect(&super::super::CollectionContext {
            frozen: &frozen,
            provider_capture: None,
            run_dir: std::path::Path::new("."),
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
        });
        assert!(
            artifacts
                .sealed_artifacts
                .iter()
                .any(|item| item.kind == "native-tool-deferred" && item.bytes == b"{}\n")
        );
    }

    #[test]
    fn final_response_requires_matching_terminal_step() {
        let text = r#"{"type":"text","part":{"messageID":"m1","text":"answer"}}"#;
        let finish = r#"{"type":"step_finish","part":{"messageID":"m1","reason":"stop"}}"#;
        let tools = r#"{"type":"step_finish","part":{"messageID":"m1","reason":"tool-calls"}}"#;
        let wrong = r#"{"type":"step_finish","part":{"messageID":"m2","reason":"stop"}}"#;
        let path = std::path::Path::new(".");
        assert!(super::final_response(&format!("{text}\n{finish}\n{{"), path).is_none());
        let session_text = r#"{"type":"text","sessionID":"s1","part":{"sessionID":"s1","messageID":"m1","text":"answer"}}"#;
        let other_session_finish = r#"{"type":"step_finish","sessionID":"s2","part":{"sessionID":"s2","messageID":"m1","reason":"stop"}}"#;
        assert!(
            super::final_response(&format!("{session_text}\n{other_session_finish}"), path)
                .is_none()
        );
        let mismatched_session = r#"{"type":"step_finish","sessionID":"s1","part":{"sessionID":"s2","messageID":"m1","reason":"stop"}}"#;
        assert!(
            super::final_response(&format!("{session_text}\n{mismatched_session}"), path).is_none()
        );
        let start = r#"{"type":"step_start","part":{"messageID":"m1"}}"#;
        assert!(
            super::final_response(&format!("{text}\n{tools}\n{start}\n{finish}"), path).is_none()
        );
        let update1 = r#"{"type":"text","part":{"id":"p1","messageID":"m1","text":"old"}}"#;
        let update2 = r#"{"type":"text","part":{"id":"p1","messageID":"m1","text":"new"}}"#;
        assert_eq!(
            super::final_response(&format!("{update1}\n{update2}\n{finish}"), path).as_deref(),
            Some("new")
        );
        let second = r#"{"type":"text","part":{"messageID":"m1","text":"second"}}"#;
        assert_eq!(
            super::final_response(&format!("{text}\n{second}\n{finish}"), path).as_deref(),
            Some("answer\nsecond")
        );
        assert_eq!(
            super::final_response(&format!("{text}\n{finish}"), path).as_deref(),
            Some("answer")
        );
        for output in [
            text.to_owned(),
            format!("{text}\n{tools}"),
            format!("{text}\n{wrong}"),
            format!("{text}\n{finish}\n{{\"type\":\"error\"}}"),
        ] {
            assert!(super::final_response(&output, path).is_none(), "{output}");
        }
    }

    #[test]
    fn incomplete_stream_does_not_count_tool_input_as_usage() {
        let output = r#"{"type":"tool_use","part":{"type":"tool","state":{"status":"completed","input":{"tokens":{"input":999,"output":888}}}}}"#;
        assert!(!super::output_usage(output, "", false).any());
    }

    #[test]
    fn task_tokens_are_not_usage_without_completed_step() {
        let output = r#"{"type":"tool_use","part":{"state":{"output":{"tokens":{"input":999,"output":888}}}}}"#;
        let usage = crate::usage::from_sources("openrouter", "opencode", output, "", None);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (None, None, None)
        );
    }
    use super::*;

    #[test]
    fn provider_configuration_is_owned_by_the_opencode_adapter() {
        let config = config(
            "test-model",
            "http://127.0.0.1:3456/v1",
            "@ai-sdk/anthropic",
            None,
        )
        .expect("configuration succeeds");
        assert!(config.contains("test-model"));
        assert!(config.contains("ASB_AGENT_API_KEY"));
    }

    #[test]
    fn tool_configuration_seals_native_tools_behind_the_asb_mcp_service() {
        let config = config(
            "test-model",
            "http://127.0.0.1:3456/v1",
            "@ai-sdk/openai-compatible",
            Some(vec!["asb-tool-replay-mcp".into(), "--tape".into()]),
        )
        .expect("configuration succeeds");
        let config: serde_json::Value = serde_json::from_str(&config).unwrap();
        assert_eq!(
            config["mcp"]["asb_local_tools"]["command"],
            serde_json::json!(["asb-tool-replay-mcp", "--tape"])
        );
        assert_eq!(config["mcp"]["asb_local_tools"]["enabled"], true);
        assert_eq!(config["mcp"]["asb_local_tools"]["timeout"], 86_400_000);
        assert_eq!(config["permission"]["bash"], "deny");
        assert_eq!(config["permission"]["asb_local_tools_*"], "allow");
    }

    #[test]
    fn preparation_pins_private_path_sorted_searches() {
        let root = std::env::temp_dir().join(format!("asb-opencode-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        let mut command = std::process::Command::new("opencode");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:3456/v1".into(),
            credential: None,
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let mut context = PreparationContext {
            command: &mut command,
            model: "fixture",
            connection: &connection,
            workspace: &root,
            runtime_dir: &root.join("runtime"),
            run_dir: &root.join("run"),
            mcp_servers: &[],
            mcp_tape: None,
            tape: None,
        };
        prepare_adapter(&mut context).unwrap();
        let config = root.join("runtime/opencode-ripgrep.conf");
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "--sort=path\n");
        assert!(command.get_envs().any(|(name, value)| {
            name == "RIPGREP_CONFIG_PATH" && value.is_some_and(|value| value == config)
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn counts_completed_jsonl_steps_without_guessing_from_final_text() {
        let output = concat!(
            "{\"type\":\"step_start\"}\n",
            "{\"type\":\"step_finish\",\"part\":{\"tokens\":{\"input\":2,\"output\":1}}}\n",
            "{\"type\":\"text\",\"part\":{\"text\":\"done\"}}\n",
            "{\"type\":\"step_finish\",\"part\":{\"tokens\":{\"input\":3,\"output\":1}}}\n"
        );
        assert_eq!(step_count(output, ""), Some(2));
        assert_eq!(step_count("{\"type\":\"text\"}\n", ""), None);
        assert_eq!(step_count("not json\n", ""), None);
    }
}
