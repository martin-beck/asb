//! Runtime extension-contract tests; no agent executable or provider runs.
use super::*;

fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "fixture".into(),
        args: vec![context.prompt.into()],
        prompt_delivery: PromptDelivery::Embedded,
    }
}
fn prepare(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    context.command.arg("prepared");
    Ok(())
}
fn adapter() -> StaticAdapter {
    StaticAdapter::new(
        "fixture",
        "fixture",
        Recipe::Host {
            executable: "fixture",
        },
        &[crate::endpoint::CHAT],
        command,
        prepare,
    )
}
fn config() -> BenchmarkConfig {
    toml::from_str("agent='fixture'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\n")
        .unwrap()
}
fn collect_context() -> CollectionContext<'static> {
    static FROZEN: std::sync::OnceLock<FrozenArtifacts> = std::sync::OnceLock::new();
    CollectionContext {
        provider_capture: None,
        frozen: FROZEN.get_or_init(FrozenArtifacts::default),
        run_dir: Path::new("fixture-run"),
        interface: None,
        anthropic_input_includes_cache: false,
        mcp_servers: &[],
        mcp_tape: None,
    }
}

#[test]
fn static_adapter_defaults_are_empty_and_preparation_command_hooks_are_called() {
    let adapter = adapter();
    assert_eq!(adapter.id(), "fixture");
    assert_eq!(adapter.executable(), "fixture");
    assert_eq!(adapter.supported_endpoints().len(), 1);
    assert!(matches!(
        adapter.recipe(),
        Recipe::Host {
            executable: "fixture"
        }
    ));
    let command = adapter.command(&CommandContext {
        executable: adapter.executable(),
        config: &config(),
        endpoint: None,
        interface: None,
        prompt: "fixture prompt",
        max_steps: None,
        native_tool_mode: None,
        native_tool: None,
    });
    assert_eq!(command.program, "fixture");
    assert_eq!(command.args, ["fixture prompt"]);
    assert_eq!(command.prompt_delivery, PromptDelivery::Embedded);
    let mut process = adapter.process("not-executed").unwrap().direct();
    let connection = crate::endpoint::Connection {
        interface: Interface::OpenAiChat,
        authentication: crate::endpoint::Authentication::Bearer,
        base_url: "http://127.0.0.1:1/v1".into(),
        credential: Some("dummy".into()),
    };
    adapter
        .prepare(&mut PreparationContext {
            command: &mut process,
            model: "fixture",
            connection: &connection,
            workspace: Path::new("not-created"),
            runtime_dir: Path::new("not-created"),
            run_dir: Path::new("not-created"),
            mcp_servers: &[],
            mcp_tape: None,
            tape: None,
        })
        .unwrap();
    assert_eq!(process.get_args().collect::<Vec<_>>(), ["prepared"]);
    assert!(adapter.collect(&collect_context()).token_usage.is_none());
    assert!(adapter.output_usage("", "", false).is_none());
    assert!(adapter.step_count("", "").is_none());
    assert!(adapter.execution_failure("", "").is_none());
    assert!(adapter.replay_credential_vars().is_empty());
}

#[test]
fn runtime_inputs_follow_the_path_command_when_no_project_installation_exists() {
    let adapter = adapter();
    let root = Path::new("/tmp/asb-agent-runtime-inputs-missing-project-tool");
    let project = crate::project::ProjectConfig::default();
    let inputs = adapter
        .runtime_inputs(&crate::runtime_inputs::Context {
            root,
            project: &project,
        })
        .unwrap();
    assert!(inputs.iter().any(|input| matches!(
        input,
        crate::runtime_inputs::Dependency::Program { path, .. } if path == Path::new("fixture")
    )));
}

#[test]
fn every_optional_hook_is_dispatched_without_changing_its_authoritative_values() {
    fn collect(_context: &CollectionContext<'_>) -> AgentArtifacts {
        AgentArtifacts {
            steps: Some(17),
            final_response: Some("frozen adapter answer".into()),
            ..Default::default()
        }
    }
    fn usage(stdout: &str, stderr: &str, includes_cache: bool) -> crate::measurements::TokenUsage {
        assert_eq!((stdout, stderr, includes_cache), ("out", "err", true));
        crate::measurements::TokenUsage {
            input_tokens: Some(123),
            output_tokens: Some(456),
            ..Default::default()
        }
    }
    fn steps(stdout: &str, stderr: &str) -> Option<u64> {
        assert_eq!((stdout, stderr), ("out", "err"));
        Some(17)
    }
    fn final_response(output: &str, path: &Path) -> Option<String> {
        Some(format!("{output}:{}", path.display()))
    }
    fn failure(stdout: &str, stderr: &str) -> Option<String> {
        Some(format!("{stdout}/{stderr}"))
    }
    fn process(executable: &str) -> Result<crate::launch::ProcessLaunch, Error> {
        let mut command = crate::launch::ProcessLaunch::new(executable);
        command.arg("hooked");
        Ok(command)
    }
    let adapter = adapter()
        .with_collection(collect)
        .with_output_usage(usage)
        .with_step_count(steps)
        .with_final_response(final_response)
        .with_execution_failure(failure)
        .with_process(process)
        .with_replay_credential_vars(&["FIXTURE_CREDENTIAL"]);
    let result = adapter.collect(&collect_context());
    assert_eq!(result.steps, Some(17));
    assert_eq!(
        result.final_response.as_deref(),
        Some("frozen adapter answer")
    );
    let usage = adapter.output_usage("out", "err", true).unwrap();
    assert_eq!(usage.input_tokens, Some(123));
    assert_eq!(usage.output_tokens, Some(456));
    assert_eq!(adapter.step_count("out", "err"), Some(17));
    assert_eq!(
        adapter
            .final_response("answer", Path::new("workspace"))
            .as_deref(),
        Some("answer:workspace")
    );
    assert_eq!(
        adapter.execution_failure("out", "err").as_deref(),
        Some("out/err")
    );
    assert_eq!(
        adapter
            .process("not-executed")
            .unwrap()
            .direct()
            .get_args()
            .collect::<Vec<_>>(),
        ["hooked"]
    );
    assert_eq!(adapter.replay_credential_vars(), ["FIXTURE_CREDENTIAL"]);
}

struct MinimalAdapter;
impl AgentAdapter for MinimalAdapter {
    fn id(&self) -> &'static str {
        "fixture"
    }
    fn executable(&self) -> &'static str {
        "fixture"
    }
    fn recipe(&self) -> Recipe {
        Recipe::Host {
            executable: "fixture",
        }
    }
    fn supported_endpoints(&self) -> &'static [Support] {
        &[]
    }
    fn command(&self, context: &CommandContext<'_>) -> CommandSpec {
        command(context)
    }
    fn prepare(&self, context: &mut PreparationContext<'_>) -> Result<(), Error> {
        prepare(context)
    }
}

#[test]
fn minimal_trait_implementation_has_conservative_optional_defaults() {
    let adapter = &MinimalAdapter as &dyn AgentAdapter;
    assert_eq!(
        adapter
            .process("not-executed")
            .unwrap()
            .direct()
            .get_program(),
        "not-executed"
    );
    assert!(adapter.replay_credential_vars().is_empty());
    let artifacts = adapter.collect(&collect_context());
    assert!(artifacts.token_usage.is_none());
    assert!(artifacts.steps.is_none());
    assert!(artifacts.final_response.is_none());
    assert!(adapter.output_usage("", "", false).is_none());
    assert!(adapter.step_count("", "").is_none());
    assert!(adapter.execution_failure("", "").is_none());
    assert!(
        adapter
            .final_response(r#"{"type":"final","text":"answer"}"#, Path::new("fixture"))
            .is_none()
    );
}

#[test]
fn unknown_preparation_does_not_mutate_command_or_create_directories() {
    let mut process = Command::new("not-executed");
    let connection = crate::endpoint::Connection {
        interface: Interface::OpenAiChat,
        authentication: crate::endpoint::Authentication::Bearer,
        base_url: "http://127.0.0.1:1/v1".into(),
        credential: Some("dummy".into()),
    };
    super::prepare(
        "not-registered",
        &mut PreparationContext {
            command: &mut process,
            model: "fixture",
            connection: &connection,
            workspace: Path::new("not-created"),
            runtime_dir: Path::new("not-created"),
            run_dir: Path::new("not-created"),
            mcp_servers: &[],
            mcp_tape: None,
            tape: None,
        },
    )
    .unwrap();
    assert_eq!(process.get_args().count(), 0);
    assert_eq!(process.get_envs().count(), 0);
}

#[test]
fn unknown_adapter_does_not_interpret_structured_answer() {
    let output = r#"{"type":"result","result":"foreign answer"}"#;
    assert!(super::final_response("not-registered", output, Path::new(".")).is_none());
}
