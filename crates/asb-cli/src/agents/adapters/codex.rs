mod native_correlation;
use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, McpTapeContext,
    PreparationContext, PromptDelivery, StaticAdapter, TapeNormalizationScript,
};
use crate::{Error, install::Recipe};
use native_correlation::bind_provider_tool_outputs;
#[cfg(test)]
use native_correlation::{decode_base64, encode_base64, provider_function_calls};

#[cfg(test)]
const NATIVE_HOOKS: &str = include_str!("../../../assets/scripts/codex-native-hooks.py");
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "codex",
    "codex",
    Recipe::Npm {
        package: "@openai/codex@0.150.1",
        executable: "codex",
        manifest: None,
    },
    &[crate::endpoint::RESPONSES],
    command,
    prepare_adapter,
)
.with_estimated_disk_mib(1024)
.with_tape_wire_extension(include_str!(
    "../../../assets/scripts/codex-native-wire.mjs"
))
.with_tape_normalization_scripts(&[TapeNormalizationScript {
    id: "codex-runtime-envelopes",
    workloads: &["*"],
    tasks: &[],
    source: include_str!("../../../assets/scripts/agent-normalizers/codex.mjs"),
}])
.with_tool_replay(prepare_tool_replay)
.with_approximate_local_tool_replay()
.with_mcp_servers()
.with_mcp_relay()
.with_native_tool_transport()
.with_guest_service(start_native_service)
.with_deferred_native_recording(finalize_native_recording)
.with_native_snapshot_check(verify_native_snapshot)
.with_collection(collect)
.with_step_count(step_count)
.with_tool_call_count(tool_call_count)
.with_output_usage(output_usage)
.with_final_response(no_stdout_final_response)
.with_artifact_specs(&[
    super::super::ArtifactSpec {
        namespace: "codex",
        filename: "native-tool-pending.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "codex-final-response",
        filename: "final.txt",
        format: super::super::ArtifactFormat::Text,
    },
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "local-tools.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "codex-native-sidecar",
        filename: "native-tool-sidecar.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "codex-native-progress",
        filename: "native-tool-progress.json",
        format: super::super::ArtifactFormat::Json,
    },
])
.with_state_environment_vars(&["CODEX_HOME"])
.with_forwarded_invocation_environment_vars(&["ASB_CODEX_SANDBOX_MODE"])
.with_preferred_executables(preferred_executables);

// Codex JSONL agent messages can be intermediate. The pinned CLI's private
// `--output-last-message` artifact is written only after a completed turn.
fn no_stdout_final_response(_: &str, _: &std::path::Path) -> Option<String> {
    None
}

fn preferred_executables(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    // The npm launcher injects package-management metadata. Prefer the native
    // package entry point, unless the project selected an explicit bin override.
    let (package, target) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => ("codex-linux-x64", "x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => ("codex-linux-arm64", "aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => ("codex-darwin-x64", "x86_64-apple-darwin"),
        ("macos", "aarch64") => ("codex-darwin-arm64", "aarch64-apple-darwin"),
        ("windows", "x86_64") => ("codex-win32-x64", "x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => ("codex-win32-arm64", "aarch64-pc-windows-msvc"),
        _ => return Vec::new(),
    };
    vec![
        root.join("node_modules/@openai")
            .join(package)
            .join("vendor")
            .join(target)
            .join("bin/codex"),
    ]
}

/// Codex's JSONL stream emits one completed `agent_message` item for each
/// completed model turn. Tool lifecycle records are deliberately excluded:
/// they describe local actions, not agent steps.
fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    let steps = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| {
            event.get("type").and_then(|value| value.as_str()) == Some("item.completed")
                && event.pointer("/item/type").and_then(|value| value.as_str())
                    == Some("agent_message")
        })
        .count()
        .try_into()
        .ok()?;
    (steps > 0).then_some(steps)
}

// Codex's JSON mode emits one completed item per executed native tool. Count
// only the documented tool item types in a complete stream; never derive
// calls from command text, nested output, or started/updated events.
fn tool_call_count(stdout: &str, _stderr: &str) -> Option<usize> {
    let mut seen_thread = false;
    let mut active_turn = false;
    let mut completed_turn = false;
    let mut calls = std::collections::HashSet::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let event: serde_json::Value = serde_json::from_str(line).ok()?;
        match event.get("type")?.as_str()? {
            "thread.started" => seen_thread = true,
            "turn.started" => {
                active_turn = true;
                completed_turn = false;
            }
            "turn.completed" => {
                if !active_turn {
                    return None;
                }
                active_turn = false;
                completed_turn = true;
            }
            "turn.failed" => return None,
            "item.completed" => {
                let item = event.get("item")?;
                match item.get("type")?.as_str()? {
                    "command_execution" | "file_change" | "mcp_tool_call" | "web_search" => {
                        if !active_turn {
                            return None;
                        }
                        if item.get("status").is_some_and(|status| {
                            !matches!(status.as_str(), Some("completed" | "failed"))
                        }) {
                            return None;
                        }
                        let id = item.get("id")?.as_str().filter(|id| !id.is_empty())?;
                        if !calls.insert(id.to_owned()) {
                            return None;
                        }
                    }
                    "agent_message" | "reasoning" | "todo_list" | "error" => {}
                    _ => return None,
                }
            }
            _ => {}
        }
    }
    (seen_thread && completed_turn && !active_turn).then_some(calls.len())
}

fn output_usage(stdout: &str, stderr: &str, _: bool) -> crate::measurements::TokenUsage {
    let mut structured = crate::measurements::TokenUsage::default();
    let mut seen = false;
    let mut native_stream = false;
    let mut cache_complete = true;
    for event in stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        let event_type = event.get("type").and_then(|value| value.as_str());
        native_stream |= matches!(
            event_type,
            Some(
                "thread.started"
                    | "turn.started"
                    | "turn.completed"
                    | "turn.failed"
                    | "item.started"
                    | "item.updated"
                    | "item.completed"
                    | "error"
            )
        );
        if event_type != Some("turn.completed") {
            continue;
        }
        let Some(value) = event.get("usage") else {
            return Default::default();
        };
        let usage = super::super::usage_fields(value);
        if usage.input_tokens.is_none() || usage.output_tokens.is_none() {
            return Default::default();
        }
        cache_complete &= usage.cached_tokens.is_some();
        structured = structured.add(usage);
        seen = true;
    }
    if seen {
        if !cache_complete {
            structured.cached_tokens = None;
        }
        return structured.with_derived_total();
    }
    if native_stream {
        // A native stream without completed usage cannot establish totals.
        return Default::default();
    }
    // Preserve legacy human-readable output while never descending into a
    // structured event's task and tool payloads.
    let mut usage = crate::measurements::TokenUsage::default();
    for text in [stdout, stderr] {
        let legacy = text
            .lines()
            .filter(|line| serde_json::from_str::<serde_json::Value>(line).is_err())
            .collect::<Vec<_>>()
            .join("\n");
        usage = usage.merge(crate::usage::usage_from_text(&legacy));
        let mut lines = text.lines();
        while let Some(line) = lines.next() {
            if serde_json::from_str::<serde_json::Value>(line).is_err()
                && line.to_ascii_lowercase().contains("tokens used")
                && let Some(value) = lines
                    .next()
                    .and_then(|next| next.trim().replace(',', "").parse::<u64>().ok())
            {
                usage.total_tokens = Some(usage.total_tokens.map_or(value, |old| old.max(value)));
            }
        }
    }
    usage.with_derived_total()
}

#[cfg(test)]
mod token_attribution_tests {
    use super::output_usage;

    #[test]
    fn tool_output_usage_cannot_override_completed_turn_usage() {
        let output = concat!(
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"aggregated_output\":\"{\\\"usage\\\":{\\\"input_tokens\\\":999,\\\"output_tokens\\\":888}}\"}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,\"cached_input_tokens\":4,\"output_tokens\":3}}\n"
        );
        let usage = output_usage(output, "", false);
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cached_tokens,
                usage.total_tokens
            ),
            (Some(10), Some(3), Some(4), Some(13))
        );
    }

    #[test]
    fn typed_turns_sum_and_missing_usage_is_unknown() {
        let first = r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":3,"cached_input_tokens":4}}"#;
        let second = r#"{"type":"turn.completed","usage":{"input_tokens":5,"output_tokens":2,"cached_input_tokens":1}}"#;
        let usage = output_usage(&format!("{first}\n{second}"), "", false);
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cached_tokens,
                usage.total_tokens
            ),
            (Some(15), Some(5), Some(5), Some(20))
        );
        let missing = r#"{"type":"turn.completed","usage":{"input_tokens":5}}"#;
        let usage = output_usage(&format!("{first}\n{missing}"), "", false);
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, usage.total_tokens),
            (None, None, None)
        );
    }
}

#[cfg(test)]
mod tool_count_tests {
    use super::tool_call_count;

    #[test]
    #[ignore = "requires a locally captured Codex JSON stream and expected count"]
    fn local_codex_stream_reports_observed_count() {
        let path = std::env::var_os("ASB_TEST_CODEX_STREAM").expect("set ASB_TEST_CODEX_STREAM");
        let expected: usize = std::env::var("ASB_TEST_CODEX_TOOL_COUNT")
            .unwrap()
            .parse()
            .unwrap();
        let stream = std::fs::read_to_string(path).unwrap();
        assert_eq!(tool_call_count(&stream, ""), Some(expected));
    }

    #[test]
    fn counts_completed_native_tools_only_in_a_complete_stream() {
        let stream = concat!(
            "{\"type\":\"thread.started\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"notice\",\"type\":\"error\"}}\n",
            "{\"type\":\"turn.started\"}\n",
            "{\"type\":\"item.started\",\"item\":{\"id\":\"1\",\"type\":\"command_execution\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"1\",\"type\":\"command_execution\",\"aggregated_output\":\"mcp_tool_call\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"2\",\"type\":\"file_change\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"3\",\"type\":\"mcp_tool_call\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"4\",\"type\":\"web_search\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"5\",\"type\":\"agent_message\"}}\n",
            "{\"type\":\"turn.completed\"}\n"
        );
        assert_eq!(tool_call_count(stream, ""), Some(4));
        assert_eq!(
            tool_call_count(&stream.replace("turn.started", "turn.unknown"), ""),
            None
        );
        assert_eq!(
            tool_call_count(&stream.replace("turn.completed", "turn.failed"), ""),
            None
        );
        assert_eq!(
            tool_call_count(&stream.replace("file_change", "unknown_tool"), ""),
            None
        );
        assert_eq!(
            tool_call_count(&stream.replace("\"id\":\"2\"", "\"id\":\"1\""), ""),
            None
        );
        assert_eq!(tool_call_count("{\"type\":\"turn.completed\"}\n", ""), None);
        assert_eq!(
            tool_call_count(
                "{\"type\":\"thread.started\"}\n{\"type\":\"turn.started\"}\n{\"type\":\"turn.completed\"}\n",
                ""
            ),
            Some(0)
        );
    }
}
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "codex".into(),
        args: vec![
            "exec".into(),
            "--skip-git-repo-check".into(),
            "--ephemeral".into(),
            "--ignore-rules".into(),
            "--json".into(),
            "--model".into(),
            context.config.model.clone(),
            "-".into(),
        ],
        prompt_delivery: PromptDelivery::Stdin,
    }
}

fn native_snapshot(mode: Option<crate::tape::Mode>) -> bool {
    match mode {
        Some(crate::tape::Mode::Record) => std::env::var_os("ASB_FS_MARKER_SOCKET").is_some(),
        Some(crate::tape::Mode::Replay) => std::env::var_os("ASB_FS_REPLAY_SOCKET").is_some(),
        None => false,
    }
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
    let bundle = std::fs::canonicalize(executable)
        .map_err(|_| Error::Run("Codex native executable is unavailable".into()))?;
    let assets = [(
        "codex-native-hook.mjs",
        include_str!("../../../assets/scripts/codex-native-hook.mjs"),
    )];
    let settings = super::super::native_guest::Settings {
        agent: "codex",
        bundle: &bundle,
        run_dir,
        mode,
        marker: marker.clone(),
        native_tool,
        snapshot: &snapshot,
        assets: &assets,
    };
    let (mut service, directory, socket) = super::super::native_guest::broker(command, &settings)?;
    let state = directory.join("hook-state");
    std::fs::create_dir_all(&state).map_err(crate::io_error)?;
    service
        .env(
            "ASB_NATIVE_SETTLEMENT_MODULE",
            directory.join("codex-native-hook.mjs"),
        )
        .env("ASB_NATIVE_SETTLEMENT_DIRECTORY", &state)
        .env("ASB_NATIVE_SETTLEMENT_MARKER", &marker);
    command
        .env("ASB_CODEX_NATIVE_DIRECTORY", state)
        .env("ASB_CODEX_NATIVE_SOCKET", socket)
        .env("ASB_CODEX_NATIVE_MARKER", marker)
        .env(
            "ASB_CODEX_NATIVE_ENTRY",
            directory.join("codex-native-hook.mjs"),
        )
        .env(
            "ASB_CODEX_NATIVE_MODE",
            if mode == crate::tape::Mode::Replay {
                "replay"
            } else {
                "record"
            },
        );
    Ok(Some(super::super::auxiliary::AuxiliaryPlan::new(service)))
}

fn finalize_native_recording(pending: &[u8], digest: &str) -> Result<Vec<u8>, Error> {
    super::super::native_guest::finalize("codex", pending, digest)
}

fn verify_native_snapshot(
    native: &[u8],
    snapshot: &mut std::fs::File,
    digest: &str,
) -> Result<(), Error> {
    super::pi::snapshot::verify_for("codex", native, snapshot, digest)
}

fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    context
        .command
        .arg("--output-last-message")
        .arg(context.run_dir.join("codex-final-response/final.txt"));
    let connection = context.connection;
    let directory = context.runtime_dir.join("codex/config");
    configure(
        &directory,
        context.model,
        &connection.base_url,
        connection.credential.is_some(),
        context.mcp_servers,
        context.mcp_tape,
        context.run_dir,
    )?;
    if native_snapshot(context.tape.map(|tape| tape.mode)) {
        let script = "node --input-type=module -e 'import(process.env.ASB_CODEX_NATIVE_ENTRY).then(m=>m.main()).catch(e=>{console.error(e.message);process.exitCode=1})'";
        let entry = serde_json::json!([{"matcher":"Bash", "hooks":[{"type":"command", "command":script, "timeout":120}]}]);
        let hooks = serde_json::json!({"hooks":{"PreToolUse":entry}});
        std::fs::write(
            directory.join("hooks.json"),
            serde_json::to_vec(&hooks).map_err(|error| {
                Error::Run(format!("cannot encode Codex native hooks: {error}"))
            })?,
        )
        .map_err(crate::io_error)?;
        let config_path = directory.join("config.toml");
        let config = std::fs::read_to_string(&config_path).map_err(crate::io_error)?;
        std::fs::write(
            &config_path,
            config.replacen("[features]\n", "[features]\ncodex_hooks = true\n", 1),
        )
        .map_err(crate::io_error)?;
        context.command.arg("--dangerously-bypass-hook-trust");
    }
    context.command.env("CODEX_HOME", &directory);
    if let Some(key) = connection.credential.as_deref() {
        context.command.env("ASB_AGENT_API_KEY", key);
    } else {
        context.command.env_remove("ASB_AGENT_API_KEY");
    }
    // Codex rejects `--approve-for-me` combined with `--sandbox`.  Its
    // approval policy also already selects the normal workspace-write mode,
    // so emit exactly one compatible noninteractive policy flag here.
    context
        .command
        .args(sandbox_args(std::env::var("ASB_CODEX_SANDBOX_MODE").ok())?);
    Ok(())
}

fn prepare_tool_replay(
    context: &mut PreparationContext<'_>,
    tape: super::super::TapeContext<'_>,
) -> Result<(), Error> {
    if native_snapshot(Some(tape.mode)) {
        return Ok(());
    }
    // The existing MCP replay transport cannot capture these native features.
    // Limit only tape runs until the native transport can replay them.
    context.command.args([
        "--disable",
        "multi_agent",
        "--disable",
        "apps",
        "--disable",
        "plugins",
    ]);
    let launch = super::super::tool_replay_mcp::allow_commuted_observations(
        super::super::tool_replay_mcp::launch(context, tape)?,
    );
    let directory = context.runtime_dir.join("codex/config");
    std::fs::create_dir_all(&directory).map_err(|error| {
        Error::Run(format!(
            "cannot create Codex local-tool replay configuration directory: {error}"
        ))
    })?;
    let mut server = toml::map::Map::new();
    server.insert(
        "command".into(),
        toml::Value::String(launch.program.to_string_lossy().into_owned()),
    );
    server.insert(
        "args".into(),
        toml::Value::Array(
            launch
                .args
                .iter()
                .map(|argument| toml::Value::String(argument.to_string_lossy().into_owned()))
                .collect(),
        ),
    );
    let mut servers = toml::map::Map::new();
    servers.insert("asb_tool_replay".into(), toml::Value::Table(server));
    let mut root = toml::map::Map::new();
    root.insert("mcp_servers".into(), toml::Value::Table(servers));
    let contents = toml::to_string(&toml::Value::Table(root)).map_err(|error| {
        Error::Run(format!(
            "cannot serialize Codex local-tool replay configuration: {error}"
        ))
    })?;
    std::fs::write(directory.join("asb-tool-replay.toml"), contents).map_err(|error| {
        Error::Run(format!(
            "cannot write Codex local-tool replay configuration: {error}"
        ))
    })?;
    // Tape runs disable Codex's native shell and use this sealed MCP server
    // as their sole tool transport. Do not register native lifecycle hooks:
    // Codex invokes them for MCP calls too, which would falsely create a
    // second native-tool recording for an already sealed MCP exchange.
    Ok(())
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let mut artifacts = super::super::mcp_relay::collect_artifacts(context);
    if let Some(pending) = context
        .frozen
        .bytes("native-tool-pending.jsonl")
        .filter(|bytes| !bytes.is_empty())
    {
        match context
            .provider_capture
            .ok_or_else(|| "Codex provider capture is unavailable".to_owned())
            .and_then(|capture| bind_provider_tool_outputs(pending, capture))
        {
            Ok(bytes) => artifacts
                .sealed_artifacts
                .push(super::super::SealedArtifact {
                    kind: "native-tool-deferred".into(),
                    bytes,
                }),
            Err(error) => artifacts.errors.push(error),
        }
    }
    artifacts.final_response = final_response_from_frozen(context.frozen);
    artifacts
}

fn final_response_from_frozen(frozen: &super::super::FrozenArtifacts) -> Option<String> {
    frozen
        .bytes("final.txt")
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}

fn sandbox_args(value: Option<String>) -> Result<&'static [&'static str], Error> {
    match value.as_deref() {
        // ASB runs agents in an owned workload guest. Avoid a second nested
        // Bubblewrap sandbox (often unavailable in container hosts) and its
        // provider-backed approval review. The external workload boundary is
        // the security boundary for unattended benchmark execution.
        None | Some("workspace-write") | Some("danger-full-access") => {
            Ok(&["--dangerously-bypass-approvals-and-sandbox"])
        }
        Some("read-only") => Err(Error::Input(
            "ASB_CODEX_SANDBOX_MODE=read-only is incompatible with an unattended coding benchmark; use workspace-write, danger-full-access, or leave it unset".into(),
        )),
        Some(value) => Err(Error::Input(format!(
            "invalid ASB_CODEX_SANDBOX_MODE `{value}`; use workspace-write, danger-full-access, or leave it unset"
        ))),
    }
}

fn configure(
    directory: &std::path::Path,
    model: &str,
    base_url: &str,
    authenticated: bool,
    mcp_servers: &[crate::benchmark::McpServerConfig],
    mcp_tape: Option<McpTapeContext<'_>>,
    run_dir: &std::path::Path,
) -> Result<(), Error> {
    std::fs::create_dir_all(directory).map_err(|error| {
        Error::Run(format!(
            "cannot create Codex provider configuration directory: {error}"
        ))
    })?;
    let model = toml::Value::String(model.to_owned());
    let base_url = toml::Value::String(base_url.to_owned());
    let key = if authenticated {
        "env_key = \"ASB_AGENT_API_KEY\"\n"
    } else {
        ""
    };
    let replay_configuration_path = directory.join("asb-tool-replay.toml");
    let replay_configuration =
        std::fs::read_to_string(&replay_configuration_path).unwrap_or_default();
    // Current Codex lifecycle hooks can observe native tools but cannot
    // replace a native function-tool result. When the tape profile is active,
    // expose only ASB's sealed MCP tool transport, which owns record/replay.
    // Ordinary live runs retain Codex's complete native tool set.
    let (search, replay_features) = if replay_configuration_path.is_file() {
        (
            "web_search = \"disabled\"\n",
            "apps = false\nplugins = false\nenable_mcp_apps = false\nshell_tool = false\n",
        )
    } else {
        ("", "")
    };
    let mcp_configuration = declared_mcp_configuration(mcp_servers, mcp_tape, run_dir)?;
    let configuration = format!(
        "model = {model}\nmodel_provider = \"asb_provider\"\n{search}\n[features]\n{replay_features}\n[model_providers.asb_provider]\nname = \"ASB provider\"\nbase_url = {base_url}\n{key}wire_api = \"responses\"\nrequires_openai_auth = false\n\n{mcp_configuration}{replay_configuration}"
    );
    std::fs::write(directory.join("config.toml"), configuration).map_err(|error| {
        Error::Run(format!(
            "cannot write Codex provider configuration: {error}"
        ))
    })
}

fn declared_mcp_configuration(
    declarations: &[crate::benchmark::McpServerConfig],
    mcp_tape: Option<McpTapeContext<'_>>,
    run_dir: &std::path::Path,
) -> Result<String, Error> {
    let mut servers = toml::map::Map::new();
    for launch in super::super::mcp_relay::server_launches(run_dir, declarations, mcp_tape)? {
        let mut server = toml::map::Map::new();
        server.insert(
            "command".into(),
            toml::Value::String(launch.program.to_string_lossy().into_owned()),
        );
        server.insert(
            "args".into(),
            toml::Value::Array(
                launch
                    .args
                    .into_iter()
                    .map(|argument| toml::Value::String(argument.to_string_lossy().into_owned()))
                    .collect(),
            ),
        );
        servers.insert(launch.id, toml::Value::Table(server));
    }
    if servers.is_empty() {
        return Ok(String::new());
    }
    let mut root = toml::map::Map::new();
    root.insert("mcp_servers".into(), toml::Value::Table(servers));
    toml::to_string(&toml::Value::Table(root))
        .map_err(|error| Error::Run(format!("cannot serialize Codex MCP configuration: {error}")))
}

#[cfg(test)]
mod tests {
    #[test]
    fn tape_wire_normalizes_only_codex_generated_input_ids() {
        let output = std::process::Command::new("node")
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/codex_native_wire.mjs"),
            )
            .output()
            .expect("node is required for the tape wire fixture");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn incomplete_native_stream_does_not_count_stderr_task_text() {
        let output = r#"{"type":"thread.started","thread_id":"test"}"#;
        assert!(!super::output_usage(output, "input tokens: 999 output tokens: 888", false).any());
    }

    use super::*;
    use crate::agents::{AgentAdapter, CollectionContext};
    use std::io::Write;

    #[test]
    fn completed_answer_comes_only_from_owned_final_message_artifact() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/codex-final-answer-tests")
            .join(crate::tools::runtime::unique_id());
        let mut owner =
            crate::agents::ArtifactOwner::prepare(&root, ADAPTER.artifact_specs()).unwrap();
        let path = root.join("codex-final-response/final.txt");
        std::fs::write(&path, "verified final answer").unwrap();
        let frozen = owner.freeze();
        assert_eq!(
            final_response_from_frozen(&frozen).as_deref(),
            Some("verified final answer")
        );
        let stdout = concat!(
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"partial answer"}}"#
        );
        assert!(ADAPTER.final_response(stdout, &root).is_none());
        drop(owner);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_empty_and_invalid_final_artifacts_do_not_invent_an_answer() {
        for (index, content) in [None, Some(&b"   "[..]), Some(&b"\xff"[..])]
            .into_iter()
            .enumerate()
        {
            let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/codex-final-answer-tests")
                .join(format!("{}-{index}", crate::tools::runtime::unique_id()));
            let mut owner =
                crate::agents::ArtifactOwner::prepare(&root, ADAPTER.artifact_specs()).unwrap();
            if let Some(content) = content {
                std::fs::write(root.join("codex-final-response/final.txt"), content).unwrap();
            }
            let frozen = owner.freeze();
            assert!(final_response_from_frozen(&frozen).is_none());
            assert_eq!(frozen.error, index == 2);
            drop(owner);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn counts_completed_agent_messages_not_tool_lifecycle_events() {
        let output = concat!(
            "{\"type\":\"turn.started\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"mcp_tool_call\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\"}}\n",
            "{\"type\":\"turn.completed\"}\n"
        );
        assert_eq!(step_count(output, ""), Some(2));
        assert_eq!(step_count("{\"type\":\"turn.completed\"}\n", ""), None);
    }

    #[test]
    fn provider_function_calls_accepts_the_sse_done_sentinel() {
        let row = serde_json::json!({
            "response": {"body": concat!(
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"exec_command\"}}\n",
                "data: [DONE]\n"
            )}
        });
        assert_eq!(
            provider_function_calls(&row).unwrap(),
            (vec![("exec_command".into(), None, None)], true)
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_package_preference_is_owned_here_and_explicit_source_still_wins() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/codex-path-fixtures")
            .join(crate::tools::runtime::unique_id());
        let installation = root.join("codex");
        let preferred = preferred_executables(&installation);
        if preferred.is_empty() {
            return;
        }
        let native = &preferred[0];
        let wrapper = installation.join("node_modules/.bin/codex");
        let explicit = installation.join("bin/codex");
        for path in [native, &wrapper] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            crate::agents::local_lookup("codex", &root).path(),
            Some(native.canonicalize().unwrap())
        );
        assert!(crate::install::tool_available("codex", &root));
        std::fs::create_dir_all(explicit.parent().unwrap()).unwrap();
        std::fs::write(&explicit, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&explicit, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            crate::agents::local_lookup("codex", &root).path(),
            Some(explicit.canonicalize().unwrap())
        );
        std::fs::remove_file(&explicit).unwrap();
        std::fs::set_permissions(native, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(crate::agents::local_lookup("codex", &root).path().is_none());
        assert!(!crate::install::tool_available("codex", &root));
        std::fs::remove_file(native).unwrap();
        assert_eq!(
            crate::agents::local_lookup("codex", &root).path(),
            Some(wrapper.canonicalize().unwrap())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_configuration_uses_safe_values_and_generic_credentials() {
        let directory =
            std::env::temp_dir().join(format!("asb-codex-config-{}", std::process::id()));
        let mcp_servers = [crate::benchmark::McpServerConfig {
            id: "filesystem".into(),
            program: "mcp-server\"fixture".into(),
            args: vec!["--workspace".into(), "./fixture workspace".into()],
        }];
        configure(
            &directory,
            "model\"name",
            "http://127.0.0.1:1234/v1",
            true,
            &mcp_servers,
            None,
            &directory,
        )
        .unwrap();
        let contents = std::fs::read_to_string(directory.join("config.toml")).unwrap();
        let config: toml::Value = toml::from_str(&contents).unwrap();
        assert!(config.get("web_search").is_none());
        for feature in ["apps", "plugins", "multi_agent", "shell_tool"] {
            assert!(
                config["features"].get(feature).is_none(),
                "{feature} should retain its normal setting"
            );
        }
        assert_eq!(config["model"].as_str(), Some("model\"name"));
        assert_eq!(config["model_provider"].as_str(), Some("asb_provider"));
        assert_eq!(
            config["model_providers"]["asb_provider"]["base_url"].as_str(),
            Some("http://127.0.0.1:1234/v1")
        );
        assert_eq!(
            config["model_providers"]["asb_provider"]["wire_api"].as_str(),
            Some("responses")
        );
        assert_eq!(
            config["model_providers"]["asb_provider"]["env_key"].as_str(),
            Some("ASB_AGENT_API_KEY")
        );
        assert_eq!(
            config["mcp_servers"]["filesystem"]["command"].as_str(),
            Some("mcp-server\"fixture")
        );
        assert_eq!(
            config["mcp_servers"]["filesystem"]["args"]
                .as_array()
                .unwrap(),
            &[
                toml::Value::String("--workspace".into()),
                toml::Value::String("./fixture workspace".into())
            ]
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn mcp_recording_substitutes_the_relay_and_seals_an_unused_server() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-mcp-record-{}",
            crate::tools::runtime::unique_id()
        ));
        let directory = root.join("config");
        let run_dir = root.join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let declarations = [crate::benchmark::McpServerConfig {
            id: "filesystem".into(),
            program: "mcp-filesystem".into(),
            args: vec!["--workspace".into(), "fixture".into()],
        }];
        configure(
            &directory,
            "fixture-model",
            "http://127.0.0.1:1234/v1",
            false,
            &declarations,
            Some(McpTapeContext {
                mode: crate::tape::Mode::Record,
                cassette: None,
            }),
            &run_dir,
        )
        .unwrap();
        let config: toml::Value =
            toml::from_str(&std::fs::read_to_string(directory.join("config.toml")).unwrap())
                .unwrap();
        let server = &config["mcp_servers"]["filesystem"];
        let args = server["args"].as_array().unwrap();
        assert_eq!(args[0].as_str(), Some("__asb-mcp-relay"));
        assert_eq!(args[1].as_str(), Some("--record-dir"));
        assert_eq!(args[3].as_str(), Some("--"));
        assert_eq!(args[4].as_str(), Some("mcp-filesystem"));
        assert!(!run_dir.join("mcp-relay/filesystem").exists());

        let artifacts = super::super::mcp_relay::collect_artifacts(&CollectionContext {
            ignore_tool_mismatch: false,
            frozen: &Default::default(),
            provider_capture: None,
            run_dir: &run_dir,
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &declarations,
            mcp_tape: Some(McpTapeContext {
                mode: crate::tape::Mode::Record,
                cassette: None,
            }),
        });
        assert!(artifacts.errors.is_empty());
        assert_eq!(artifacts.sealed_artifacts.len(), 1);
        assert_eq!(artifacts.sealed_artifacts[0].kind, "mcp-relay-filesystem");
        let replay = root.join("replay");
        super::super::mcp_relay::materialize_bundle(&artifacts.sealed_artifacts[0].bytes, &replay)
            .unwrap();
        super::super::mcp_relay::validate_recording(&replay).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mcp_replay_materializes_only_the_sealed_relay_bundle() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-mcp-replay-{}",
            crate::tools::runtime::unique_id()
        ));
        let directory = root.join("config");
        let run_dir = root.join("run");
        let batch = root.join("batch");
        std::fs::create_dir_all(&batch).unwrap();
        let cassette = batch.join("replay.jsonl");
        std::fs::write(&cassette, "{}\n").unwrap();
        std::fs::write(
            batch.join("mcp-relay-filesystem.jsonl"),
            super::super::mcp_relay::empty_bundle().unwrap(),
        )
        .unwrap();
        let declarations = [crate::benchmark::McpServerConfig {
            id: "filesystem".into(),
            program: "must-not-run".into(),
            args: vec!["--live".into()],
        }];
        configure(
            &directory,
            "fixture-model",
            "http://127.0.0.1:1234/v1",
            false,
            &declarations,
            Some(McpTapeContext {
                mode: crate::tape::Mode::Replay,
                cassette: Some(&cassette),
            }),
            &run_dir,
        )
        .unwrap();
        let config: toml::Value =
            toml::from_str(&std::fs::read_to_string(directory.join("config.toml")).unwrap())
                .unwrap();
        let server = &config["mcp_servers"]["filesystem"];
        assert_ne!(server["command"].as_str(), Some("must-not-run"));
        assert_eq!(server["args"][0].as_str(), Some("__asb-mcp-relay"));
        assert_eq!(server["args"][1].as_str(), Some("--replay-dir"));
        super::super::mcp_relay::validate_recording(&run_dir.join("mcp-replay/filesystem"))
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn mcp_replay_refuses_a_missing_selected_relay_bundle() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-mcp-replay-missing-{}",
            crate::tools::runtime::unique_id()
        ));
        let batch = root.join("batch");
        std::fs::create_dir_all(&batch).unwrap();
        let cassette = batch.join("replay.jsonl");
        std::fs::write(&cassette, "{}\n").unwrap();
        let declarations = [crate::benchmark::McpServerConfig {
            id: "filesystem".into(),
            program: "must-not-run".into(),
            args: vec!["--live".into()],
        }];
        let error = declared_mcp_configuration(
            &declarations,
            Some(McpTapeContext {
                mode: crate::tape::Mode::Replay,
                cassette: Some(&cassette),
            }),
            &root.join("run"),
        )
        .expect_err("missing selected relay bundle must not launch the declared server");
        assert!(
            error
                .to_string()
                .contains("MCP relay bundle is unavailable")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sandbox_override_selects_one_compatible_noninteractive_policy() {
        assert_eq!(
            sandbox_args(None).unwrap(),
            ["--dangerously-bypass-approvals-and-sandbox"]
        );
        assert_eq!(
            sandbox_args(Some("workspace-write".into())).unwrap(),
            ["--dangerously-bypass-approvals-and-sandbox"]
        );
        assert_eq!(
            sandbox_args(Some("danger-full-access".into())).unwrap(),
            ["--dangerously-bypass-approvals-and-sandbox"]
        );
        assert!(sandbox_args(Some("read-only".into())).is_err());
        assert!(sandbox_args(Some("unsafe".into())).is_err());
    }

    #[test]
    fn command_delivers_the_workload_prompt_only_through_controlled_stdin() {
        let config: crate::benchmark::BenchmarkConfig = toml::from_str(
            "agent='codex'\nprovider='fixture'\nmodel='fixture'\nworkload='fixture'\n",
        )
        .unwrap();
        let spec = command(&CommandContext {
            executable: "codex",
            config: &config,
            endpoint: None,
            interface: None,
            prompt: "private fixture prompt",
            max_steps: None,
            native_tool_mode: None,
            native_tool: None,
        });
        assert_eq!(spec.prompt_delivery, PromptDelivery::Stdin);
        assert_eq!(spec.args.last().map(String::as_str), Some("-"));
        assert!(!spec.args.iter().any(|argument| argument == "--disable"));
        assert!(
            !spec
                .args
                .iter()
                .any(|argument| argument == "private fixture prompt")
        );
    }

    #[test]
    fn snapshot_tape_keeps_codex_native_features_and_configures_hooks() {
        const CHILD: &str = "ASB_TEST_CODEX_SNAPSHOT_PREPARATION";
        if std::env::var_os(CHILD).is_none() {
            for (value, variable) in [
                ("record", "ASB_FS_MARKER_SOCKET"),
                ("replay", "ASB_FS_REPLAY_SOCKET"),
            ] {
                let status=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","agents::adapters::codex::tests::snapshot_tape_keeps_codex_native_features_and_configures_hooks"]).env(CHILD,value).env(variable,"/fixture/socket").status().unwrap();
                assert!(status.success());
            }
            return;
        }
        let mode = if std::env::var(CHILD).unwrap() == "record" {
            crate::tape::Mode::Record
        } else {
            crate::tape::Mode::Replay
        };
        let root = std::env::temp_dir().join(format!(
            "asb-codex-snapshot-{}",
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let cassette = root.join("recording.jsonl");
        let mut cmd = std::process::Command::new("codex");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiResponses,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("fixture".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        ADAPTER
            .prepare(&mut PreparationContext {
                command: &mut cmd,
                model: "fixture",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &root,
                run_dir: &root,
                mcp_servers: &[],
                mcp_tape: None,
                tape: Some(crate::agents::TapeContext {
                    ignore_tool_mismatch: false,
                    mode,
                    cassette: &cassette,
                }),
            })
            .unwrap();
        let config: toml::Value = toml::from_str(
            &std::fs::read_to_string(root.join("codex/config/config.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(config["features"]["codex_hooks"].as_bool(), Some(true));
        for feature in ["shell_tool", "multi_agent", "apps", "plugins"] {
            assert_ne!(
                config["features"]
                    .get(feature)
                    .and_then(toml::Value::as_bool),
                Some(false)
            );
        }
        assert!(!root.join("codex/config/asb-tool-replay.toml").exists());
        assert!(root.join("codex/config/hooks.json").is_file());
        assert!(!cmd.get_args().any(|arg| arg == "--disable"));
        let native = root.join("native.jsonl");
        assert!(
            start_native_service(
                &mut cmd,
                "/bin/true",
                &root,
                Some(mode),
                Some(&native),
                Some(&"a".repeat(64))
            )
            .unwrap()
            .is_some()
        );
        assert!(
            cmd.get_envs()
                .any(|(name, value)| name == "ASB_CODEX_NATIVE_SOCKET" && value.is_some())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tape_backed_preparation_uses_the_private_mcp_replay_server() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-tool-replay-{}",
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let cassette = root.join("recording.jsonl");
        let mcp_servers = [crate::benchmark::McpServerConfig {
            id: "filesystem".into(),
            program: "mcp-filesystem".into(),
            args: vec![".".into()],
        }];
        let mut command = std::process::Command::new("codex");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiResponses,
            base_url: "http://127.0.0.1:4321/v1".into(),
            credential: Some("fixture-key".into()),
            authentication: crate::endpoint::Authentication::Bearer,
        };
        ADAPTER
            .prepare(&mut PreparationContext {
                command: &mut command,
                model: "fixture-model",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &root,
                run_dir: &root,
                mcp_servers: &mcp_servers,
                mcp_tape: None,
                tape: Some(crate::agents::TapeContext {
                    ignore_tool_mismatch: false,
                    mode: crate::tape::Mode::Record,
                    cassette: &cassette,
                }),
            })
            .unwrap();
        let args = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let config: toml::Value = toml::from_str(
            &std::fs::read_to_string(root.join("codex/config/config.toml")).unwrap(),
        )
        .unwrap();
        let server = &config["mcp_servers"]["asb_tool_replay"];
        assert!(server["command"].as_str().is_some());
        assert!(
            server["args"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str() == Some("__asb-tool-replay"))
        );
        assert_eq!(
            config["mcp_servers"]["filesystem"]["command"].as_str(),
            Some("mcp-filesystem")
        );
        assert_eq!(config["model"].as_str(), Some("fixture-model"));
        assert_eq!(config["features"]["shell_tool"].as_bool(), Some(false));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--disable", "multi_agent"])
        );
        assert!(config.get("hooks").is_none());
        assert!(
            !args
                .iter()
                .any(|argument| argument == "--dangerously-bypass-hook-trust")
        );
        assert!(
            !root
                .join("codex-native-progress/native-tool-progress.json")
                .exists(),
            "the adapter must leave its declared namespace for the owner"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn invoke_native_hook(
        script: &std::path::Path,
        mode: &str,
        sidecar: &std::path::Path,
        progress: &std::path::Path,
        state: &std::path::Path,
        operation: &str,
        input: Option<&str>,
    ) -> std::process::Output {
        let mut command = std::process::Command::new("python3");
        command
            .arg(script)
            .arg(operation)
            .env("ASB_CODEX_NATIVE_MODE", mode)
            .env("ASB_CODEX_NATIVE_SIDECAR", sidecar)
            .env("ASB_CODEX_NATIVE_PROGRESS", progress)
            .env("ASB_CODEX_NATIVE_STATE", state);
        if input.is_some() {
            command.stdin(std::process::Stdio::piped());
        }
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = command.spawn().unwrap();
        if let Some(input) = input {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    }

    #[test]
    fn native_hook_seals_and_replays_when_codex_omits_post_tool_use() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-native-hook-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("hook.py");
        std::fs::write(&script, NATIVE_HOOKS).unwrap();
        let sidecar = root.join("sidecar.jsonl");
        let progress = root.join("progress.json");
        let state = root.join("state.json");
        let marker = root.join("unexpected-native-execution");
        let pre = format!(
            r#"{{"tool_name":"Bash","session_id":"record-session","tool_use_id":"call-1","tool_input":{{"command":"touch {}"}}}}"#,
            marker.display()
        );
        assert!(
            invoke_native_hook(
                &script,
                "record",
                &sidecar,
                &progress,
                &state,
                "pre",
                Some(&pre)
            )
            .status
            .success()
        );
        assert!(
            invoke_native_hook(
                &script, "record", &sidecar, &progress, &state, "finish", None
            )
            .status
            .success()
        );
        let rows = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(rows.lines().count(), 7);
        let capture = serde_json::json!({
            "request": {"body": serde_json::json!({
                "input": [{"call_id": "call-1", "output": "Chunk ID: recorded\nWall time: 0.2164 seconds\nProcess exited with code 0\nOriginal token count: 7\nOutput:\nfixture"}]
            }).to_string()},
            "agent_record_metadata":{"conversation":"record-session","output_call_ids":["call-1"]}
        });
        // Provider request snapshots can retain the same already-completed
        // call in later turns. Equal historical rows represent one exchange.
        std::fs::write(
            root.join("capture.jsonl"),
            format!("{capture}\n{capture}\n"),
        )
        .unwrap();
        std::fs::write(
            &sidecar,
            bind_provider_tool_outputs(rows.as_bytes(), &root.join("capture.jsonl")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&progress).unwrap())
                .unwrap()["complete"],
            true
        );
        std::fs::remove_file(&state).unwrap();
        let replay_pre = pre
            .replace("call-1", "fresh-replay-id")
            .replace("record-session", "replay-session");
        let replay = invoke_native_hook(
            &script,
            "replay",
            &sidecar,
            &progress,
            &state,
            "pre",
            Some(&replay_pre),
        );
        assert!(replay.status.success());
        let value: serde_json::Value = serde_json::from_slice(&replay.stdout).unwrap();
        let command = value["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains(" emit 0"));
        assert!(
            !command.contains(marker.to_str().unwrap()),
            "the replay wrapper must not execute the recorded native command"
        );
        let emitted = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .env("ASB_CODEX_NATIVE_MODE", "replay")
            .env("ASB_CODEX_NATIVE_SIDECAR", &sidecar)
            .env("ASB_CODEX_NATIVE_PROGRESS", &progress)
            .env("ASB_CODEX_NATIVE_STATE", &state)
            .output()
            .unwrap();
        assert!(emitted.status.success());
        assert_eq!(String::from_utf8(emitted.stdout).unwrap(), "fixture");
        assert!(
            emitted.stderr.is_empty(),
            "normal emitter exit must not report an error"
        );
        assert!(
            !marker.exists(),
            "the recorded native command must never run in replay"
        );
        let replay_post = invoke_native_hook(
            &script,
            "replay",
            &sidecar,
            &progress,
            &state,
            "post",
            Some(
                r#"{"tool_name":"Bash","session_id":"replay-session","tool_use_id":"fresh-replay-id","tool_response":"ignored-emitter-output"}"#,
            ),
        );
        assert!(replay_post.status.success());
        assert!(replay_post.stdout.is_empty());
        assert!(
            invoke_native_hook(
                &script, "replay", &sidecar, &progress, &state, "finish", None
            )
            .status
            .success()
        );
        let mismatch = invoke_native_hook(
            &script,
            "replay",
            &sidecar,
            &progress,
            &state,
            "pre",
            Some(
                r#"{"tool_name":"Bash","tool_use_id":"call-2","tool_input":{"command":"different"}}"#,
            ),
        );
        assert!(!mismatch.status.success());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn native_hook_tracks_same_call_id_in_independent_sessions() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-hook-sessions-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("hook.py");
        std::fs::write(&script, NATIVE_HOOKS).unwrap();
        let sidecar = root.join("sidecar.jsonl");
        let progress = root.join("progress.json");
        let state = root.join("state.json");
        for session in ["one", "two"] {
            let event = serde_json::json!({"session_id":session,"tool_use_id":"same","tool_name":"Bash","tool_input":{"command":"true"}}).to_string();
            assert!(
                invoke_native_hook(
                    &script,
                    "record",
                    &sidecar,
                    &progress,
                    &state,
                    "pre",
                    Some(&event)
                )
                .status
                .success()
            );
        }
        for session in ["two", "one"] {
            let event =
                serde_json::json!({"session_id":session,"tool_use_id":"same","tool_response":""})
                    .to_string();
            assert!(
                invoke_native_hook(
                    &script,
                    "record",
                    &sidecar,
                    &progress,
                    &state,
                    "post",
                    Some(&event)
                )
                .status
                .success()
            );
        }
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(state).unwrap()).unwrap();
        assert!(value["pending"].as_object().unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_emitter_preserves_nonzero_status_without_reporting_hook_failure() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-emitter-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("hook.py");
        let state = root.join("state.json");
        let progress = root.join("progress.json");
        std::fs::write(&script, NATIVE_HOOKS).unwrap();
        let response = serde_json::to_vec(&"Chunk ID: recorded\nWall time: 0.01 seconds\nProcess exited with code 7\nOutput:\nfixture").unwrap();
        std::fs::write(
            &state,
            serde_json::json!({"pending": {}, "next": 0, "replay": [["", encode_base64(&response)]]}).to_string(),
        )
        .unwrap();
        let output = std::process::Command::new("python3")
            .arg(&script)
            .args(["emit", "0"])
            .env("ASB_CODEX_NATIVE_MODE", "replay")
            .env("ASB_CODEX_NATIVE_SIDECAR", root.join("sidecar.jsonl"))
            .env("ASB_CODEX_NATIVE_PROGRESS", &progress)
            .env("ASB_CODEX_NATIVE_STATE", &state)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"fixture");
        assert!(output.stderr.is_empty());
        assert!(!progress.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn provider_correlation_consumes_cumulative_same_id_history_in_order() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-provider-history-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let request = |id: &str| {
            encode_base64(
                serde_json::to_string(&serde_json::json!({"tool_use_id": id}))
                    .unwrap()
                    .as_bytes(),
            )
        };
        let sidecar = [
            serde_json::json!({"event":"write","request_b64":request("call-1")}),
            serde_json::json!({"event":"output"}),
            serde_json::json!({"event":"write","request_b64":request("call-1")}),
            serde_json::json!({"event":"output"}),
            serde_json::json!({"event":"workspace_effect"}),
        ]
        .into_iter()
        .map(|row| serde_json::to_string(&row).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        let first = serde_json::json!({"request":{"body":serde_json::json!({"input":[{"call_id":"call-1","output":"first"}]}).to_string()}});
        let second = serde_json::json!({"request":{"body":serde_json::json!({"input":[{"call_id":"call-1","output":"first"},{"call_id":"call-1","output":"second"}]}).to_string()}});
        let capture = root.join("capture.jsonl");
        std::fs::write(&capture, format!("{first}\n{second}\n")).unwrap();
        let bound = bind_provider_tool_outputs(sidecar.as_bytes(), &capture).unwrap();
        let rows = String::from_utf8(bound)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_ne!(rows[1]["output_b64"], rows[3]["output_b64"]);
        let mismatched = serde_json::json!({"request":{"body":serde_json::json!({"input":[
            {"call_id":"different","output":"first"},
            {"call_id":"call-1","output":"second"}
        ]}).to_string()}});
        std::fs::write(&capture, format!("{mismatched}\n")).unwrap();
        assert_eq!(
            bind_provider_tool_outputs(sidecar.as_bytes(), &capture).unwrap_err(),
            "Codex provider capture has no matching native tool result"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_correlation_ignores_non_native_operations() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-provider-call-kinds-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let request = encode_base64(br#"{"tool_use_id":"call-1"}"#);
        let sidecar = [
            serde_json::json!({"event":"write","request_b64":request}),
            serde_json::json!({"event":"output"}),
            serde_json::json!({"event":"workspace_effect"}),
        ]
        .into_iter()
        .map(|row| serde_json::to_string(&row).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        let response = |name: &str| {
            format!(
                "data: {}\n",
                serde_json::json!({
                    "type":"response.output_item.done",
                    "item":{"type":"function_call","name":name}
                })
            )
        };
        let row = |input: serde_json::Value, response: String| {
            serde_json::json!({
                "request":{"body":serde_json::json!({"input":input}).to_string()},
                "response":{"body":response}
            })
        };
        let capture = root.join("capture.jsonl");
        let first = row(serde_json::json!([]), response("exec_command"));
        let second = row(
            serde_json::json!([{"call_id":"call-1","output":"native"}]),
            response("write_stdin"),
        );
        let third = row(
            serde_json::json!([
                {"call_id":"call-1","output":"native"},
                {"call_id":"same","output":"stream"}
            ]),
            response("update_plan"),
        );
        let fourth = row(
            serde_json::json!([
                {"call_id":"call-1","output":"native"},
                {"call_id":"same","output":"stream"},
                {"call_id":"plan","output":"updated"}
            ]),
            String::new(),
        );
        std::fs::write(&capture, format!("{first}\n{second}\n{third}\n{fourth}\n")).unwrap();
        let bound = bind_provider_tool_outputs(sidecar.as_bytes(), &capture).unwrap();
        let rows = String::from_utf8(bound)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let output = decode_base64(rows[1]["output_b64"].as_str().unwrap()).unwrap();
        assert_eq!(output, br#""native""#);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_correlation_attaches_running_process_followup_to_its_owner() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-followup-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let response = |name: &str, id: &str, args: serde_json::Value| {
            format!(
                "data: {}\n",
                serde_json::json!({"type":"response.output_item.done","item":{"type":"function_call","name":name,"call_id":id,"arguments":args.to_string()}})
            )
        };
        let row = |input: serde_json::Value,
                   response: String,
                   ids: serde_json::Value,
                   waits: serde_json::Value,
                   sessions: serde_json::Value| serde_json::json!({"request":{"body":serde_json::json!({"input":input}).to_string()},"response":{"body":response},"agent_record_metadata":{"conversation":"thread","output_call_ids":ids,"running_wait_ms":waits,"running_session_ids":sessions}});
        let running = "Chunk ID: <asb-chunk>\nWall time: <asb-wall-time> seconds\nProcess running with session ID <asb-session>\nOutput:\npartial";
        let result = "Chunk ID: <asb-chunk>\nWall time: <asb-wall-time> seconds\nProcess exited with code 0\nOutput:\nlater";
        let a = row(
            serde_json::json!([]),
            response("exec_command", "first", serde_json::json!({})),
            serde_json::json!([]),
            serde_json::json!([]),
            serde_json::json!([]),
        );
        let b = row(
            serde_json::json!([{"call_id":"normalized","output":running}]),
            response(
                "write_stdin",
                "second",
                serde_json::json!({"session_id":42}),
            ),
            serde_json::json!(["first"]),
            serde_json::json!([1250]),
            serde_json::json!(["42"]),
        );
        let c = row(
            serde_json::json!([{"call_id":"normalized","output":running},{"call_id":"normalized","output":result}]),
            String::new(),
            serde_json::json!(["first", "second"]),
            serde_json::json!([1250, null]),
            serde_json::json!(["42", null]),
        );
        let capture = root.join("capture.jsonl");
        std::fs::write(&capture, format!("{a}\n{b}\n{c}\n")).unwrap();
        let request = encode_base64(br#"{"ordinal":0,"request":{"tool":"Bash","input":{}}}"#);
        let outcome =
            encode_base64(br#"{"ok":true,"value":{"session_id":"thread","tool_use_id":"first"}}"#);
        let pending = [
            serde_json::json!({"event":"write","request_b64":request}),
            serde_json::json!({"event":"output","output_b64":outcome}),
            serde_json::json!({"event":"exit"}),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        let bound =
            String::from_utf8(bind_provider_tool_outputs(pending.as_bytes(), &capture).unwrap())
                .unwrap();
        let output: serde_json::Value =
            serde_json::from_str(bound.lines().nth(1).unwrap()).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&decode_base64(output["output_b64"].as_str().unwrap()).unwrap())
                .unwrap();
        assert_eq!(value["value"]["running_wait_ms"], 1250);
        assert_eq!(value["value"]["followups"][0]["output"], result);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_correlation_matches_parallel_results_by_original_identity() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-parallel-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let request =
            |id: &str| encode_base64(serde_json::json!({"tool_use_id":id}).to_string().as_bytes());
        let sidecar = [
            serde_json::json!({"event":"write","request_b64":request("call-a")}),
            serde_json::json!({"event":"output"}),
            serde_json::json!({"event":"write","request_b64":request("call-b")}),
            serde_json::json!({"event":"output"}),
            serde_json::json!({"event":"workspace_effect"}),
        ]
        .into_iter()
        .map(|row| row.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        let response = ["call-a", "call-b"].into_iter().map(|id| format!("data: {}\n", serde_json::json!({"type":"response.output_item.done","item":{"type":"function_call","name":"exec_command","call_id":id}}))).collect::<String>();
        let first = serde_json::json!({"request":{"body":serde_json::json!({"input":[]}).to_string()},"response":{"body":response},"agent_record_metadata":{"schema_version":1,"conversation":"thread-a","output_call_ids":[]}});
        let second = serde_json::json!({"request":{"body":serde_json::json!({"input":[{"call_id":"<asb-codex-input-call-id>","output":"result-b"},{"call_id":"<asb-codex-input-call-id>","output":"result-a"}]}).to_string()},"response":{"body":""},"agent_record_metadata":{"schema_version":1,"conversation":"thread-a","output_call_ids":["call-b","call-a"]}});
        let capture = root.join("capture.jsonl");
        std::fs::write(&capture, format!("{first}\n{second}\n")).unwrap();
        let rows =
            String::from_utf8(bind_provider_tool_outputs(sidecar.as_bytes(), &capture).unwrap())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>();
        assert_eq!(
            decode_base64(rows[1]["output_b64"].as_str().unwrap()).unwrap(),
            br#""result-a""#
        );
        assert_eq!(
            decode_base64(rows[3]["output_b64"].as_str().unwrap()).unwrap(),
            br#""result-b""#
        );
        let changed = second.to_string().replace("call-b", "call-other");
        std::fs::write(&capture, format!("{first}\n{changed}\n")).unwrap();
        assert!(bind_provider_tool_outputs(sidecar.as_bytes(), &capture).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn broker_recording_binds_provider_results_before_snapshot_finalization() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-broker-bind-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let pending = root.join("pending.jsonl");
        let script =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/codex_native_hook.mjs");
        assert!(
            std::process::Command::new("node")
                .arg(script)
                .env("ASB_CODEX_PENDING_FIXTURE", &pending)
                .status()
                .unwrap()
                .success()
        );
        let begin = |thread: &str| serde_json::json!({"request":{"body":"{\"input\":[]}"},"response":{"body":format!("data: {}\n",serde_json::json!({"type":"response.output_item.done","item":{"type":"function_call","name":"exec_command","call_id":"same-id"}}))},"agent_record_metadata":{"conversation":thread,"output_call_ids":[]}});
        let end = |thread: &str, output: &str| serde_json::json!({"request":{"body":serde_json::json!({"input":[{"call_id":"normalized","output":output}]}).to_string()},"response":{"body":""},"agent_record_metadata":{"conversation":thread,"output_call_ids":["same-id"]}});
        let capture = root.join("capture.jsonl");
        std::fs::write(
            &capture,
            format!(
                "{}\n{}\n{}\n{}\n",
                begin("agent-a"),
                begin("agent-b"),
                end("agent-b", "B"),
                end("agent-a", "A")
            ),
        )
        .unwrap();
        let bound = bind_provider_tool_outputs(&std::fs::read(pending).unwrap(), &capture).unwrap();
        let completed =
            super::super::super::native_guest::finalize("codex", &bound, &"f".repeat(64)).unwrap();
        assert!(
            std::str::from_utf8(&completed)
                .unwrap()
                .contains("workspace_effect")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_correlation_keeps_interleaved_conversations_separate() {
        let root = std::env::temp_dir().join(format!(
            "asb-codex-conversations-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let request =
            |id: &str| encode_base64(serde_json::json!({"tool_use_id":id}).to_string().as_bytes());
        let sidecar = [
            serde_json::json!({"event":"write","request_b64":request("call-a")}),
            serde_json::json!({"event":"output"}),
            serde_json::json!({"event":"write","request_b64":request("call-b")}),
            serde_json::json!({"event":"output"}),
            serde_json::json!({"event":"workspace_effect"}),
        ]
        .into_iter()
        .map(|row| row.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        let begin = |thread: &str, id: &str| serde_json::json!({"request":{"body":"{\"input\":[]}"},"response":{"body":format!("data: {}\n",serde_json::json!({"type":"response.output_item.done","item":{"type":"function_call","name":"exec_command","call_id":id}}))},"agent_record_metadata":{"schema_version":1,"conversation":thread,"output_call_ids":[]}});
        let end = |thread: &str, id: &str, output: &str| serde_json::json!({"request":{"body":serde_json::json!({"input":[{"call_id":"<asb-codex-input-call-id>","output":output}]}).to_string()},"response":{"body":""},"agent_record_metadata":{"schema_version":1,"conversation":thread,"output_call_ids":[id]}});
        let capture = root.join("capture.jsonl");
        std::fs::write(
            &capture,
            format!(
                "{}\n{}\n{}\n{}\n",
                begin("a", "call-a"),
                begin("b", "call-b"),
                end("b", "call-b", "B"),
                end("a", "call-a", "A")
            ),
        )
        .unwrap();
        let rows =
            String::from_utf8(bind_provider_tool_outputs(sidecar.as_bytes(), &capture).unwrap())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>();
        assert_eq!(
            decode_base64(rows[1]["output_b64"].as_str().unwrap()).unwrap(),
            br#""A""#
        );
        assert_eq!(
            decode_base64(rows[3]["output_b64"].as_str().unwrap()).unwrap(),
            br#""B""#
        );
        std::fs::write(
            &capture,
            format!(
                "{}\n{}\n{}\n{}\n",
                begin("a", "same"),
                begin("b", "same"),
                end("b", "same", "B"),
                end("a", "same", "A")
            ),
        )
        .unwrap();
        let ambiguous = [
            serde_json::json!({"event":"write","request_b64":request("same")}),
            serde_json::json!({"event":"output"}),
            serde_json::json!({"event":"workspace_effect"}),
        ]
        .into_iter()
        .map(|row| row.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        assert_eq!(
            bind_provider_tool_outputs(ambiguous.as_bytes(), &capture).unwrap_err(),
            "Codex provider call identity is missing or ambiguous"
        );
        let tagged = |session: &str| {
            encode_base64(
                serde_json::json!({"session_id":session,"tool_use_id":"same"})
                    .to_string()
                    .as_bytes(),
            )
        };
        let tagged_sidecar = [
            serde_json::json!({"event":"write","request_b64":tagged("a")}),
            serde_json::json!({"event":"output"}),
            serde_json::json!({"event":"write","request_b64":tagged("b")}),
            serde_json::json!({"event":"output"}),
            serde_json::json!({"event":"workspace_effect"}),
        ]
        .into_iter()
        .map(|row| row.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        let tagged_rows = String::from_utf8(
            bind_provider_tool_outputs(tagged_sidecar.as_bytes(), &capture).unwrap(),
        )
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
        assert_eq!(
            decode_base64(tagged_rows[1]["output_b64"].as_str().unwrap()).unwrap(),
            br#""A""#
        );
        assert_eq!(
            decode_base64(tagged_rows[3]["output_b64"].as_str().unwrap()).unwrap(),
            br#""B""#
        );
        let broker_request = encode_base64(
            br#"{"ordinal":0,"request":{"tool":"Bash","input":{}},"snapshot_sequence":2}"#,
        );
        let broker_outcome =
            encode_base64(br#"{"ok":true,"value":{"session_id":"a","tool_use_id":"same"}}"#);
        let broker_request_b = encode_base64(
            br#"{"ordinal":1,"request":{"tool":"Bash","input":{}},"snapshot_sequence":3}"#,
        );
        let broker_outcome_b =
            encode_base64(br#"{"ok":true,"value":{"session_id":"b","tool_use_id":"same"}}"#);
        let pending = [
            serde_json::json!({"event":"write","request_b64":broker_request}),
            serde_json::json!({"event":"output","output_b64":broker_outcome}),
            serde_json::json!({"event":"write","request_b64":broker_request_b}),
            serde_json::json!({"event":"output","output_b64":broker_outcome_b}),
            serde_json::json!({"event":"exit"}),
        ]
        .into_iter()
        .map(|row| row.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        let rows =
            String::from_utf8(bind_provider_tool_outputs(pending.as_bytes(), &capture).unwrap())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .collect::<Vec<_>>();
        let output = |index: usize| {
            serde_json::from_slice::<serde_json::Value>(
                &decode_base64(rows[index]["output_b64"].as_str().unwrap()).unwrap(),
            )
            .unwrap()
        };
        assert_eq!(
            output(1),
            serde_json::json!({"ok":true,"value":{"output":"A","running_wait_ms":null,"followups":[]}})
        );
        assert_eq!(
            output(3),
            serde_json::json!({"ok":true,"value":{"output":"B","running_wait_ms":null,"followups":[]}})
        );
        assert_eq!(rows.last().unwrap()["event"], "exit");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires a local Codex provider capture with correlation metadata"]
    fn local_provider_capture_correlates_all_native_calls() {
        let path = std::path::PathBuf::from(
            std::env::var_os("ASB_TEST_CODEX_CAPTURE").expect("set ASB_TEST_CODEX_CAPTURE"),
        );
        let capture = std::fs::read_to_string(&path).unwrap();
        let mut sidecar = Vec::new();
        let mut count = 0;
        for line in capture.lines() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let (calls, _) = provider_function_calls(&row).unwrap();
            let session = row
                .pointer("/agent_record_metadata/conversation")
                .and_then(serde_json::Value::as_str)
                .expect("recorded conversation identity");
            for (name, id, _) in calls {
                if name != "exec_command" {
                    continue;
                }
                let id = id.expect("provider call identity");
                let request = encode_base64(
                    serde_json::json!({"session_id":session,"tool_use_id":id})
                        .to_string()
                        .as_bytes(),
                );
                sidecar
                    .push(serde_json::json!({"event":"write","request_b64":request}).to_string());
                sidecar.push(serde_json::json!({"event":"output"}).to_string());
                count += 1;
            }
        }
        assert!(count > 0);
        sidecar.push(serde_json::json!({"event":"workspace_effect"}).to_string());
        let bound =
            bind_provider_tool_outputs((sidecar.join("\n") + "\n").as_bytes(), &path).unwrap();
        assert!(!bound.is_empty());
    }
}
