//! Native command preparation is complete before process gating or launch.
use super::*;

pub(super) struct NativeRoute<'a> {
    pub connection: Option<&'a crate::endpoint::Connection>,
    pub provider_endpoint: Option<&'a str>,
    pub provider_credentials: Option<(&'a str, &'a str)>,
    pub tape_url: Option<&'a str>,
    pub tape_mode: Option<tape::Mode>,
    pub max_steps: Option<u64>,
    pub tool_tape_mode: Option<tape::Mode>,
    pub tool_tape_cassette: Option<&'a Path>,
    pub mcp_tape_mode: Option<tape::Mode>,
    pub mcp_tape_cassette: Option<&'a Path>,
}

/// Immutable intent, not shared mutable runtime state.
pub(super) struct NativePlan<'a> {
    pub config_path: &'a Path,
    pub config: &'a BenchmarkConfig,
    pub project_root: &'a Path,
    pub invocation_dir: &'a Path,
    pub project_config: &'a project::ProjectConfig,
    pub invocation: &'a workloads::session::TaskInvocation,
    /// Explicit repeated attempts isolate agent state before adapter setup.
    pub fresh_state: bool,
    pub route: NativeRoute<'a>,
}

pub(super) struct PreparedNative {
    artifact_owner: agents::ArtifactOwner,
    command: Command,
    route: crate::launch::CommandRoute,
    run_dir: PathBuf,
    runtime_path: PathBuf,
    execution_budget: Duration,
    stdin_prompt: Option<Vec<u8>>,
    /// Held by the spawning runner until the gated child has inherited it.
    /// The gate turns the descriptor close-on-exec before starting the client.
    #[cfg(unix)]
    gate_trace: Option<std::fs::File>,
}

impl NativePlan<'_> {
    pub fn prepare(
        self,
        measurements: Option<&mut measurements::Session>,
    ) -> Result<PreparedNative, Error> {
        let Self {
            config_path,
            config,
            project_root,
            invocation_dir,
            project_config,
            invocation,
            fresh_state,
            route,
        } = self;
        let NativeRoute {
            connection,
            provider_endpoint,
            provider_credentials,
            tape_url,
            tape_mode,
            max_steps,
            tool_tape_mode,
            tool_tape_cassette,
            mcp_tape_mode,
            mcp_tape_cassette,
        } = route;
        let workspace = &invocation.workspace;
        invocation
            .input
            .validate_for_agent(&config.agent)
            .map_err(Error::Input)?;
        let resolved = crate::planning::resolve_for_run(config, measurements.is_some())?;
        if resolved.backend != "native" {
            return Err(Error::Input(
                "native execution cannot substitute for the selected module backend".into(),
            ));
        }
        let execution_budget = Duration::from_secs(
            resolved
                .budgets
                .execution_seconds
                .unwrap_or(crate::planning::DEFAULT_NATIVE_EXECUTION_SECONDS),
        );
        let prompt = &invocation.input.instructions;
        // Adapters run with the benchmark workspace as their current directory.
        // Pass the canonical workspace explicitly so a relative configuration
        // value is not resolved a second time inside that directory (which is
        // especially important for OpenJiuwen's persistent workspace).
        let mut command_config = config.clone();
        command_config.workspace = workspace.to_string_lossy().into_owned();
        let specification = command_for_with_interface(
            &command_config,
            connection
                .as_ref()
                .map(|connection| connection.base_url.as_str())
                .or(provider_endpoint),
            prompt,
            connection.as_ref().map(|connection| connection.interface),
            max_steps,
        )?;
        let mut program = specification.program;
        match agents::execution_lookup_at(
            &config.agent,
            project_root,
            &project_config.tools_dir,
            invocation_dir,
        ) {
            crate::executables::Lookup::Found(local) => {
                program = local.to_string_lossy().into_owned();
            }
            crate::executables::Lookup::Missing => {}
            crate::executables::Lookup::Unusable(path) => {
                return Err(Error::Input(format!(
                    "selected agent executable {} is not a usable executable file; repair the installation or explicit override before running",
                    path.display()
                )));
            }
        }
        let launch = agents::find(&config.agent).map_or_else(
            || Ok(crate::launch::ProcessLaunch::new(&program)),
            |adapter| crate::agents::select_process(adapter, &program, None),
        )?;
        #[cfg(unix)]
        let gate =
            if (measurements.is_some() || cfg!(target_os = "linux"))
                && gate_target_available(&launch.program)
            {
                Some(crate::process_gate::executable().map_err(|error| {
                    Error::Run(format!("cannot resolve ASB process gate: {error}"))
                })?)
            } else {
                None
            };
        #[cfg(not(unix))]
        let gate = None;
        let (mut command, route) = launch.native(gate.as_deref());
        project::apply_network_proxy(&mut command, config_path)?;
        command.args(specification.args);
        // Do not leak an enclosing Codex session into the benchmark agent. These
        // variables are host-runner integration state; if inherited, a project
        // local Codex CLI can advertise the host's tools as an
        // `additional_tools` request that cli2key does not support.
        agents::driver::isolate_parent_state(&mut command);
        let stdin_prompt = match specification.prompt_delivery {
            agents::PromptDelivery::Argument => {
                command.arg(prompt);
                None
            }
            agents::PromptDelivery::Embedded => None,
            agents::PromptDelivery::Stdin => Some(prompt.as_bytes().to_vec()),
        };
        command
            .current_dir(workspace)
            .stdin(if stdin_prompt.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        providers::configure(
            &mut command,
            &config.provider,
            &config.model,
            tape_mode == Some(tape::Mode::Replay),
        );
        if tape_mode == Some(tape::Mode::Replay) {
            agents::mask_replay_credentials(&mut command);
        }
        providers::apply_with_tape(
            &mut command,
            &config.provider,
            &config.model,
            provider_credentials,
            tape_url,
            tape_mode == Some(tape::Mode::Replay),
        )?;
        // Agent state must not appear inside the workload checkout, where it can
        // affect git status, tools, and therefore the recorded agent trajectory.
        let runtime_path = replica::runtime_path(workspace);
        // Run-specific agent artifacts must not be confused with earlier runs or
        // simultaneous replicas, including after process IDs are reused.
        static RUN_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let run_dir = runtime_path.join("runs").join(format!(
            "{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            RUN_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        ));
        let mut artifact_specs = agents::find(&config.agent)
            .map_or_else(Vec::new, |adapter| adapter.artifact_specs().to_vec());
        artifact_specs.push(agents::ArtifactSpec {
            namespace: "native-trace",
            filename: "native-trace.jsonl",
            format: agents::ArtifactFormat::JsonLines,
        });
        let artifact_owner = agents::ArtifactOwner::prepare(&run_dir, &artifact_specs)?;
        instrument_phase(measurements, LifecyclePhase::AgentPreparation, || {
            if fresh_state {
                agents::driver::prepare_fresh_state(&mut command, &run_dir.join("state"))?;
            }
            if let Some(connection) = connection.as_ref() {
                agents::prepare(
                    &config.agent,
                    &mut agents::PreparationContext {
                        command: &mut command,
                        model: &config.model,
                        connection,
                        workspace,
                        runtime_dir: &runtime_path,
                        run_dir: &run_dir,
                        mcp_servers: &config.mcp_servers,
                        mcp_tape: mcp_tape_mode.map(|mode| agents::McpTapeContext {
                            mode,
                            cassette: mcp_tape_cassette,
                        }),
                        tape: tool_tape_mode
                            .zip(tool_tape_cassette)
                            .map(|(mode, cassette)| agents::TapeContext { mode, cassette }),
                    },
                )?;
            }
            Ok(())
        })?;
        // Adapter preparation may contribute a deterministic host event to the
        // same trace.  Open the append-only gate channel only after it has
        // finished, so a descriptor never races a truncating adapter write.
        #[cfg(unix)]
        let gate_trace = gate
            .is_some()
            .then(|| agents::native_trace::prepare_gate_channel(&mut command, &run_dir))
            .transpose()?;
        if let Some(url) = tape_url {
            providers::configure_tape(
                &mut command,
                &config.provider,
                url,
                tape_mode.expect("tape URL implies tape mode"),
            );
        }
        if tape_mode == Some(tape::Mode::Replay) {
            apply_replay_network_guard(&mut command);
        }
        project::apply_python_path(&mut command, project_root, Path::new(&program))?;
        crate::tools::apply_runtime_path(&mut command, project_root, &project_config.tools_dir)?;

        Ok(PreparedNative {
            artifact_owner,
            command,
            route,
            run_dir,
            runtime_path,
            execution_budget,
            stdin_prompt,
            #[cfg(unix)]
            gate_trace,
        })
    }
}

impl PreparedNative {
    /// Hash the frozen native target only when an explicit capsule needs it.
    /// Ordinary live execution therefore pays no extra executable I/O.
    pub fn root_executable(&self) -> Result<agents::native_trace::Blob, Error> {
        agents::capsule::executable_identity(&self.route.target(&self.command))
    }

    pub fn execute(
        mut self,
        measurements: Option<&mut measurements::Session>,
        show_output: bool,
        output: &mut dyn Write,
        launch: Option<&dyn workloads::session::LaunchReadiness>,
        progress: progress::Context,
    ) -> Result<(ExecutionEvidence, AttemptIssues), Error> {
        // The process gate must execute the fully prepared command, not the
        // adapter's original executable. Some adapters (for example OpenJiuwen)
        // install a process wrapper that carries compatibility and output fixes;
        // bypassing it at the gate would silently undo those fixes while still
        // appearing to attach collectors correctly.
        // Keep the parent descriptor alive across spawn.  The gate receives a
        // duplicate during exec, then seals it before launching the client.
        #[cfg(unix)]
        let _gate_trace = self.gate_trace.as_ref();
        let target = self.route.target(&self.command);
        let child = run_measured_agent_with_launch_progress(
            &mut self.command,
            &target,
            Some(self.route),
            measurements,
            show_output,
            output,
            progress,
            self.stdin_prompt.as_deref(),
            NativeLaunch {
                readiness: launch,
                runtime: self.execution_budget,
            },
        )?;
        agents::native_trace::write_root_lifecycle(&self.run_dir)?;
        Ok((
            ExecutionEvidence {
                observation: child.observation,
                artifact_owner: Some(self.artifact_owner),
                capture_complete: child.issues.capture_error.is_none(),
                output: child.output,
                run_dir: self.run_dir,
                guest: None,
                elapsed_ms: child.elapsed_ms,
                runtime_output: Some(self.runtime_path),
            },
            child.issues,
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn codex_native_preference_retains_fresh_session_configuration() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/native-plan-fixtures")
            .join(workloads::TaskIdentity::local("codex-native", None).attempt_id);
        let adapter = agents::find("codex").unwrap();
        let Some(tool) = adapter
            .preferred_executables(&root.join("tools/codex"))
            .into_iter()
            .next()
        else {
            return; // The adapter declares no native package on this platform.
        };
        std::fs::create_dir_all(tool.parent().unwrap()).unwrap();
        std::fs::write(&tool, "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config: BenchmarkConfig = toml::from_str(
            "agent='codex'\nprovider='openrouter'\nmodel='fixture'\nworkload='fixture'\n",
        )
        .unwrap();
        let project = project::ProjectConfig {
            tools_dir: "tools".into(),
            ..Default::default()
        };
        let invocation = workloads::session::TaskInvocation {
            identity: workloads::TaskIdentity::local("fixture", None),
            input: "private fixture prompt".into(),
            workspace: root.join("workspace"),
        };
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiResponses,
            authentication: crate::endpoint::Authentication::Bearer,
            base_url: "http://127.0.0.1:1/v1".into(),
            credential: Some("asb-replay".into()),
        };
        let prepare = || {
            NativePlan {
                config_path: &root.join("benchmark.toml"),
                config: &config,
                project_root: &root,
                invocation_dir: &root,
                project_config: &project,
                invocation: &invocation,
                fresh_state: true,
                route: NativeRoute {
                    connection: Some(&connection),
                    provider_endpoint: Some(&connection.base_url),
                    provider_credentials: None,
                    tape_url: Some("http://127.0.0.1:1"),
                    tape_mode: Some(tape::Mode::Replay),
                    max_steps: None,
                    tool_tape_mode: None,
                    tool_tape_cassette: None,
                    mcp_tape_mode: None,
                    mcp_tape_cassette: None,
                },
            }
            .prepare(None)
        };
        let prepared = prepare().unwrap();
        assert_eq!(
            prepared.route.target(&prepared.command).program,
            tool.canonicalize().unwrap().as_os_str()
        );
        let first_identity = prepared.root_executable().unwrap();
        assert_eq!(
            first_identity.bytes,
            std::fs::metadata(&tool).unwrap().len()
        );
        let vars: std::collections::HashMap<_, _> = prepared.command.get_envs().collect();
        let home = prepared.run_dir.join("state/home");
        let codex_home = prepared.runtime_path.join("codex/config");
        assert_eq!(vars[std::ffi::OsStr::new("HOME")], Some(home.as_os_str()));
        assert_eq!(
            vars[std::ffi::OsStr::new("CODEX_HOME")],
            Some(codex_home.as_os_str())
        );
        assert!(home.is_dir());
        assert!(codex_home.join("config.toml").is_file());
        assert!(!codex_home.starts_with(&invocation.workspace));
        assert_eq!(vars[std::ffi::OsStr::new("CODEX_SESSION_ID")], None);
        assert_eq!(vars[std::ffi::OsStr::new("CODEX_TOOL_CALL_ID")], None);
        assert_eq!(vars[std::ffi::OsStr::new("CODEX_VERSION")], None);
        let explicit = root.join("tools/codex/bin/codex");
        std::fs::create_dir_all(explicit.parent().unwrap()).unwrap();
        std::fs::write(&explicit, "#!/bin/sh\nexit 98\n").unwrap();
        std::fs::set_permissions(&explicit, std::fs::Permissions::from_mode(0o700)).unwrap();
        let overridden = prepare().unwrap();
        assert_eq!(
            overridden.route.target(&overridden.command).program,
            explicit.canonicalize().unwrap().as_os_str()
        );
        assert_ne!(overridden.root_executable().unwrap(), first_identity);
        // A broken explicit override cannot silently choose the valid native package.
        std::fs::set_permissions(&explicit, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            prepare()
                .err()
                .unwrap()
                .to_string()
                .contains("not a usable executable")
        );
        std::fs::remove_file(&explicit).unwrap();
        // Nor may a broken native package become a host-PATH launch.
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            prepare()
                .err()
                .unwrap()
                .to_string()
                .contains("not a usable executable")
        );
        std::fs::remove_file(&tool).unwrap();
        let missing = prepare().unwrap();
        assert_eq!(missing.route.target(&missing.command).program, "codex");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prepared_command_keeps_adapter_wrapper_route_and_replay_guard_before_gating() {
        for fresh_state in [false, true] {
            prepared_command(fresh_state);
        }
    }

    fn prepared_command(fresh_state: bool) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/native-plan-fixtures")
            .join(workloads::TaskIdentity::local("native", None).attempt_id);
        let tool = root.join("tools/openjiuwen/bin/openjiuwen");
        let interpreter = root.join("fixture/python3");
        std::fs::create_dir_all(tool.parent().unwrap()).unwrap();
        std::fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
        std::fs::write(&interpreter, b"fixture interpreter; never executed").unwrap();
        std::fs::write(
            &tool,
            format!(
                "#!{}\nfixture client; never executed\n",
                interpreter.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config: BenchmarkConfig = toml::from_str(
            "agent='openjiuwen'\nprovider='openrouter'\nmodel='fixture'\nworkload='fixture'\n",
        )
        .unwrap();
        let project = project::ProjectConfig {
            tools_dir: "tools".into(),
            ..Default::default()
        };
        let invocation = workloads::session::TaskInvocation {
            identity: workloads::TaskIdentity::local("fixture", None),
            input: "private fixture prompt".into(),
            workspace: root.join("workspace"),
        };
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::AnthropicMessages,
            authentication: crate::endpoint::Authentication::ApiKey,
            base_url: "http://127.0.0.1:1/v1".into(),
            credential: Some("asb-replay".into()),
        };
        let prepared = NativePlan {
            config_path: &root.join("benchmark.toml"),
            config: &config,
            project_root: &root,
            invocation_dir: &root,
            project_config: &project,
            invocation: &invocation,
            fresh_state,
            route: NativeRoute {
                connection: Some(&connection),
                provider_endpoint: Some(&connection.base_url),
                provider_credentials: None,
                tape_url: Some("http://127.0.0.1:1"),
                tape_mode: Some(tape::Mode::Replay),
                max_steps: None,
                tool_tape_mode: None,
                tool_tape_cassette: None,
                mcp_tape_mode: None,
                mcp_tape_cassette: None,
            },
        }
        .prepare(None)
        .unwrap();
        let target = prepared.route.target(&prepared.command);
        assert_eq!(target.program, interpreter.as_os_str());
        let args: Vec<_> = target
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "-c");
        assert!(
            args[1].contains("runpy"),
            "adapter compatibility wrapper must remain the gated target"
        );
        assert_eq!(Path::new(&args[2]), tool.canonicalize().unwrap());
        assert_eq!(
            prepared.command.get_current_dir(),
            Some(invocation.workspace.as_path())
        );
        let vars: std::collections::HashMap<_, _> = prepared.command.get_envs().collect();
        assert_eq!(
            vars[std::ffi::OsStr::new("HTTPS_PROXY")],
            Some(std::ffi::OsStr::new("http://127.0.0.1:9"))
        );
        assert_eq!(
            vars[std::ffi::OsStr::new("NO_PROXY")],
            Some(std::ffi::OsStr::new("127.0.0.1,localhost,::1"))
        );
        assert_eq!(vars[std::ffi::OsStr::new("CODEX_SESSION_ID")], None);
        let home = vars.get(std::ffi::OsStr::new("HOME"));
        if fresh_state {
            let expected = prepared.run_dir.join("state/home");
            assert_eq!(home, Some(&Some(expected.as_os_str())));
            assert!(expected.is_dir());
        } else {
            assert!(home.is_none(), "legacy runs inherit the user's home");
        }
        assert!(!prepared.run_dir.starts_with(&invocation.workspace));
        std::fs::remove_dir_all(root).unwrap();
    }
}
