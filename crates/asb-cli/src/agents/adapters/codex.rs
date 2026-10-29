use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, McpTapeContext,
    PreparationContext, PromptDelivery, StaticAdapter,
};
use crate::{Error, install::Recipe};

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
.with_tool_replay(prepare_tool_replay)
.with_mcp_servers()
.with_mcp_relay()
.with_native_tool_transport()
.with_collection(collect)
.with_artifact_specs(&[
    super::super::ArtifactSpec {
        namespace: "tool-replay",
        filename: "local-tools.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "codex",
        filename: "native-tool-sidecar.jsonl",
        format: super::super::ArtifactFormat::JsonLines,
    },
    super::super::ArtifactSpec {
        namespace: "codex",
        filename: "native-tool-progress.json",
        format: super::super::ArtifactFormat::Json,
    },
])
.with_state_environment_vars(&["CODEX_HOME"])
.with_forwarded_invocation_environment_vars(&["ASB_CODEX_SANDBOX_MODE"])
.with_preferred_executables(preferred_executables);

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
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "codex".into(),
        args: vec![
            "exec".into(),
            "--skip-git-repo-check".into(),
            "--ephemeral".into(),
            "--ignore-rules".into(),
            "--disable".into(),
            "multi_agent".into(),
            "--disable".into(),
            "apps".into(),
            "--disable".into(),
            "plugins".into(),
            "--json".into(),
            "--model".into(),
            context.config.model.clone(),
            "-".into(),
        ],
        prompt_delivery: PromptDelivery::Stdin,
    }
}

fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
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
    let launch = super::super::tool_replay_mcp::launch(context, tape)?;
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
    let mut contents = toml::to_string(&toml::Value::Table(root)).map_err(|error| {
        Error::Run(format!(
            "cannot serialize Codex local-tool replay configuration: {error}"
        ))
    })?;
    let hook_dir = context.runtime_dir.join("codex-native-hooks");
    std::fs::create_dir_all(&hook_dir).map_err(|error| {
        Error::Run(format!(
            "cannot create Codex native hook directory: {error}"
        ))
    })?;
    let hook = hook_dir.join("hook.py");
    std::fs::write(&hook, NATIVE_HOOKS)
        .map_err(|error| Error::Run(format!("cannot write Codex native hook: {error}")))?;
    let mode = match tape.mode {
        crate::tape::Mode::Record => "record",
        crate::tape::Mode::Replay => "replay",
    };
    let artifact_dir = context.run_dir.join("codex");
    let sidecar = match tape.mode {
        crate::tape::Mode::Record => artifact_dir.join("native-tool-sidecar.jsonl"),
        crate::tape::Mode::Replay => context.runtime_dir.join("native-tool-sidecar.jsonl"),
    };
    let progress = artifact_dir.join("native-tool-progress.json");
    let state = context.runtime_dir.join("codex-native-hook-state.json");
    let command = hook_command(&hook)?;
    contents.push_str(&native_hook_configuration(&command)?);
    std::fs::write(directory.join("asb-tool-replay.toml"), contents).map_err(|error| {
        Error::Run(format!(
            "cannot write Codex local-tool replay configuration: {error}"
        ))
    })?;
    // Retain Codex's built-in workspace shell. Some Responses-compatible
    // providers do not surface configured MCP tools to the model; disabling
    // the shell in that case leaves a coding benchmark unable to read, edit,
    // or test its assigned repository. The workload guest remains the
    // execution boundary, while the tape still records the provider turns.
    context
        .command
        .arg("--dangerously-bypass-hook-trust")
        .env("ASB_CODEX_NATIVE_MODE", mode)
        .env("ASB_CODEX_NATIVE_SIDECAR", sidecar)
        .env("ASB_CODEX_NATIVE_PROGRESS", progress)
        .env("ASB_CODEX_NATIVE_STATE", state);
    Ok(())
}

/// Codex currently exposes local shell and patch calls through its documented
/// lifecycle hooks. The adapter owns their payload schema and the replay shim;
/// the runner sees only the completed opaque native sidecar.
fn hook_command(path: &std::path::Path) -> Result<String, Error> {
    let value = path
        .to_str()
        .ok_or_else(|| Error::Run("Codex native hook path is not valid UTF-8".into()))?;
    if value.contains(['\0', '\n', '\r']) {
        return Err(Error::Run("Codex native hook path is unsafe".into()));
    }
    Ok(format!("python3 '{}'", value.replace('\'', "'\\\"'\\\"'")))
}

fn native_hook_configuration(command: &str) -> Result<String, Error> {
    let quoted = |operation| toml::Value::String(format!("{command} {operation}")).to_string();
    Ok(format!(
        "\n[[hooks.PreToolUse]]\nmatcher = \".*\"\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = {}\n\n[[hooks.PostToolUse]]\nmatcher = \".*\"\n[[hooks.PostToolUse.hooks]]\ntype = \"command\"\ncommand = {}\n\n[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = {}\n",
        quoted("pre"),
        quoted("post"),
        quoted("finish"),
    ))
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let mut artifacts = super::super::mcp_relay::collect_artifacts(context);
    let Some(progress) = context.frozen.document("native-tool-progress.json") else {
        artifacts
            .errors
            .push("Codex native tool hook did not publish a completion receipt".into());
        return artifacts;
    };
    let complete = progress
        .get("complete")
        .and_then(serde_json::Value::as_bool);
    let mode = progress.get("mode").and_then(serde_json::Value::as_str);
    if progress
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
        || complete != Some(true)
        || !matches!(mode, Some("record") | Some("replay"))
    {
        artifacts
            .errors
            .push("Codex native tool hook did not complete".into());
        return artifacts;
    }
    if mode == Some("record") {
        let Some(sidecar) = context.frozen.bytes("native-tool-sidecar.jsonl") else {
            artifacts
                .errors
                .push("Codex native tool recording is unavailable".into());
            return artifacts;
        };
        artifacts.native_tool_sidecar = Some(sidecar.to_vec());
    }
    artifacts
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
    let replay_configuration =
        std::fs::read_to_string(directory.join("asb-tool-replay.toml")).unwrap_or_default();
    let mcp_configuration = declared_mcp_configuration(mcp_servers, mcp_tape, run_dir)?;
    let configuration = format!(
        "model = {model}\nmodel_provider = \"asb_provider\"\nweb_search = \"disabled\"\n\n[features]\napps = false\nplugins = false\nenable_mcp_apps = false\n\n[model_providers.asb_provider]\nname = \"ASB provider\"\nbase_url = {base_url}\n{key}wire_api = \"responses\"\nrequires_openai_auth = false\n\n{mcp_configuration}{replay_configuration}"
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
    use super::*;
    use crate::agents::{AgentAdapter, CollectionContext};
    use std::io::Write;
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
            frozen: &Default::default(),
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
            native_tool_mode: None,
            native_tool: None,
        });
        assert_eq!(spec.prompt_delivery, PromptDelivery::Stdin);
        assert_eq!(spec.args.last().map(String::as_str), Some("-"));
        assert!(
            !spec
                .args
                .iter()
                .any(|argument| argument == "private fixture prompt")
        );
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
                    mode: crate::tape::Mode::Record,
                    cassette: &cassette,
                }),
            })
            .unwrap();
        let args = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(
            !args
                .windows(2)
                .any(|pair| pair == ["--disable", "shell_tool"]),
            "the workspace shell must remain available when a provider does not expose MCP tools"
        );
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
        assert_eq!(
            config["hooks"]["PreToolUse"][0]["matcher"].as_str(),
            Some(".*")
        );
        assert!(
            args.iter()
                .any(|argument| argument == "--dangerously-bypass-hook-trust")
        );
        assert!(
            !root.join("codex/native-tool-progress.json").exists(),
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
    fn native_hook_records_and_replays_only_the_adapter_wire() {
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
        let pre = r#"{"tool_name":"Bash","tool_use_id":"call-1","tool_input":{"command":"printf fixture"}}"#;
        let post = r#"{"tool_name":"Bash","tool_use_id":"call-1","tool_response":{"stdout":"fixture","exit_code":0}}"#;
        assert!(
            invoke_native_hook(
                &script,
                "record",
                &sidecar,
                &progress,
                &state,
                "pre",
                Some(pre)
            )
            .status
            .success()
        );
        assert!(
            invoke_native_hook(
                &script,
                "record",
                &sidecar,
                &progress,
                &state,
                "post",
                Some(post)
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
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&progress).unwrap())
                .unwrap()["complete"],
            true
        );
        std::fs::remove_file(&state).unwrap();
        let replay = invoke_native_hook(
            &script,
            "replay",
            &sidecar,
            &progress,
            &state,
            "pre",
            Some(pre),
        );
        assert!(replay.status.success());
        let value: serde_json::Value = serde_json::from_slice(&replay.stdout).unwrap();
        let command = value["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains(" emit 0"));
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
        assert_eq!(String::from_utf8(emitted.stdout).unwrap(), "fixture\n");
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
}
