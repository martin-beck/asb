use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, McpTapeContext,
    PreparationContext, PromptDelivery, StaticAdapter,
};
use crate::{Error, install::Recipe};
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
.with_collection(collect)
.with_artifact_specs(&[super::super::ArtifactSpec {
    namespace: "tool-replay",
    filename: "local-tools.jsonl",
    format: super::super::ArtifactFormat::JsonLines,
}])
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
    // Retain Codex's built-in workspace shell. Some Responses-compatible
    // providers do not surface configured MCP tools to the model; disabling
    // the shell in that case leaves a coding benchmark unable to read, edit,
    // or test its assigned repository. The workload guest remains the
    // execution boundary, while the tape still records the provider turns.
    Ok(())
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
    mcp_tape: Option<McpTapeContext>,
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
    mcp_tape: Option<McpTapeContext>,
    run_dir: &std::path::Path,
) -> Result<String, Error> {
    if declarations.is_empty() {
        return Ok(String::new());
    }
    let mut servers = toml::map::Map::new();
    for declaration in declarations {
        let launch = match mcp_tape {
            Some(McpTapeContext {
                mode: crate::tape::Mode::Record,
            }) => Some(super::super::mcp_relay::record_launch(
                run_dir,
                declaration,
            )?),
            Some(McpTapeContext {
                mode: crate::tape::Mode::Replay,
            }) => {
                return Err(Error::Run(
                    "Codex MCP replay is not available until its sealed relay input is staged"
                        .into(),
                ));
            }
            None => None,
        };
        let mut server = toml::map::Map::new();
        server.insert(
            "command".into(),
            toml::Value::String(
                launch
                    .as_ref()
                    .map(|launch| launch.program.to_string_lossy().into_owned())
                    .unwrap_or_else(|| declaration.program.clone()),
            ),
        );
        server.insert(
            "args".into(),
            toml::Value::Array(launch.map_or_else(
                || {
                    declaration
                        .args
                        .iter()
                        .cloned()
                        .map(toml::Value::String)
                        .collect()
                },
                |launch| {
                    launch
                        .args
                        .into_iter()
                        .map(|argument| {
                            toml::Value::String(argument.to_string_lossy().into_owned())
                        })
                        .collect()
                },
            )),
        );
        servers.insert(declaration.id.clone(), toml::Value::Table(server));
    }
    let mut root = toml::map::Map::new();
    root.insert("mcp_servers".into(), toml::Value::Table(servers));
    toml::to_string(&toml::Value::Table(root))
        .map_err(|error| Error::Run(format!("cannot serialize Codex MCP configuration: {error}")))
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let mut artifacts = AgentArtifacts::default();
    if context.mcp_tape.map(|tape| tape.mode) != Some(crate::tape::Mode::Record) {
        return artifacts;
    }
    for declaration in context.mcp_servers {
        let recording = context.run_dir.join("mcp-relay").join(&declaration.id);
        let bundle = if recording.exists() {
            super::super::mcp_relay::bundle_recording(&recording)
        } else {
            super::super::mcp_relay::empty_bundle()
        };
        match bundle {
            Ok(bytes) => artifacts
                .sealed_artifacts
                .push(super::super::SealedArtifact {
                    kind: format!("mcp-relay-{}", declaration.id),
                    bytes,
                }),
            Err(error) => artifacts.errors.push(format!(
                "cannot seal MCP relay recording for `{}`: {error}",
                declaration.id
            )),
        }
    }
    artifacts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentAdapter;
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

        let artifacts = collect(&CollectionContext {
            frozen: &Default::default(),
            run_dir: &run_dir,
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &declarations,
            mcp_tape: Some(McpTapeContext {
                mode: crate::tape::Mode::Record,
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
            config: &config,
            endpoint: None,
            interface: None,
            prompt: "private fixture prompt",
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
        let _ = std::fs::remove_dir_all(root);
    }
}
