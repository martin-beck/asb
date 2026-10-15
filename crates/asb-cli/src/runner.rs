//! Live execution of one configured benchmark.

use super::{Error, io_error, read_config};
use crate::benchmark::BenchmarkConfig;
use crate::cli2key;
use crate::measurements;
use crate::models;
use crate::prepare;
use crate::project;
use crate::providers;
use crate::suggestions;
use crate::tape;
use crate::workloads;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

const MAX_OUTPUT: usize = 1024 * 1024;
const BUILTIN_AGENTS: &[&str] = &[
    "codex",
    "pi",
    "opencode",
    "codeagent",
    "workbuddy",
    "gemini",
    "qwen-code",
    "aider",
    "mini-swe",
    "openhands",
    "openjiuwen",
];

#[derive(Serialize)]
struct RunResult<'a> {
    status: &'a str,
    agent: &'a str,
    workload: &'a str,
    provider: &'a str,
    model: &'a str,
    success_rate: f64,
    elapsed_ms: u128,
    exit_code: Option<i32>,
    token_usage: measurements::TokenUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    cassette: Option<String>,
    stdout: String,
    stderr: String,
}

/// Execute one configured benchmark, optionally through a local llmtape proxy.
pub fn run_with_tape(
    path: &str,
    json: bool,
    measurement_dir: Option<&str>,
    mode: Option<tape::Mode>,
    cassette: Option<&str>,
    output: &mut dyn Write,
) -> Result<(), Error> {
    let tape_request = mode.map(|value| (value, cassette));
    run_mode(path, json, measurement_dir, tape_request, output)
}

fn run_mode(
    path: &str,
    json: bool,
    measurement_dir: Option<&str>,
    tape_request: Option<(tape::Mode, Option<&str>)>,
    output: &mut dyn Write,
) -> Result<(), Error> {
    let config_path = Path::new(path);
    let text = read_config(config_path)?;
    let config: BenchmarkConfig = toml::from_str(&text)
        .map_err(|error| Error::Input(format!("cannot parse {path}: {error}")))?;
    let tape_cassette = tape_request.map(|(mode, requested)| {
        (
            mode,
            requested
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| default_cassette(config_path, &config)),
        )
    });
    if let Some((tape::Mode::Replay, cassette)) = tape_cassette.as_ref() {
        models::ensure_replay_tool_capable(cassette, &config.model)?;
    } else {
        models::ensure_tool_capable(&config.provider, &config.model)?;
    }
    let (project_root, project_config) = project::load(config_path)?;
    let project_root = project_root.canonicalize().map_err(|error| {
        Error::Input(format!(
            "cannot resolve benchmark project directory: {error}"
        ))
    })?;
    let workspace_name = if config.workspace == "asb-workspace" {
        &project_config.workspace_dir
    } else {
        &config.workspace
    };
    let workspace = project::resolve(&project_root, workspace_name);
    std::fs::create_dir_all(&workspace).map_err(|error| {
        Error::Run(format!(
            "cannot create workspace {}: {error}",
            workspace.display()
        ))
    })?;
    let workload_directory = std::env::var_os("ASB_WORKLOAD_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| project::resolve(&project_root, &project_config.workloads_dir));
    let prompt = workloads::load(&config.workload, &workload_directory).map_err(Error::Input)?;
    workloads::prepare_fixture(&config.workload, &workspace).map_err(Error::Run)?;
    workloads::prepare_external(&config.workload, &workload_directory, &workspace)
        .map_err(Error::Run)?;
    if config.provider.eq_ignore_ascii_case("cli2key")
        && matches!(config.agent.as_str(), "aider" | "gemini" | "workbuddy")
    {
        return Err(Error::Run(format!(
            "cli2key cannot run agent {}: the local bridge exposes Anthropic Messages and OpenAI Responses, while this agent requires a different provider protocol; use codex, opencode, qwen-code, mini-swe, openhands, or openjiuwen",
            config.agent
        )));
    }
    let codex_executable = local_agent("codex", &project_root, &project_config.tools_dir);
    let tape_mode = tape_request.map(|(mode, _)| mode);
    let bridge = if config.provider.eq_ignore_ascii_case("cli2key")
        && tape_mode != Some(tape::Mode::Replay)
    {
        cli2key::Bridge::connect(
            local_agent("cli2key", &project_root, &project_config.tools_dir).as_deref(),
        )
        .map(Some)?
    } else {
        None
    };
    let mut tape_session = if let Some((mode, cassette)) = tape_cassette.as_ref() {
        let upstream = tape::upstream(
            &config.provider,
            bridge.as_ref().map(cli2key::Bridge::base_url),
        );
        let tape_executable = local_agent("llmtape", &project_root, &project_config.tools_dir);
        Some(tape::Session::start(
            *mode,
            cassette,
            upstream.as_deref(),
            tape_executable.as_deref().map(Path::new),
        )?)
    } else {
        None
    };
    let tape_url = tape_session.as_ref().map(|session| session.url());
    let specification = command_for(
        &config,
        tape_url.or_else(|| bridge.as_ref().map(cli2key::Bridge::base_url)),
        &prompt,
    )?;
    let mut program = specification.program;
    if let Some(local) = local_agent(&config.agent, &project_root, &project_config.tools_dir) {
        program = local;
    }
    let started = Instant::now();
    let mut command = Command::new(&program);
    command.args(specification.args);
    if specification.append_prompt {
        command.arg(prompt);
    }
    command
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    providers::configure(&mut command, &config.provider, &config.model);
    let cli2key_endpoint = tape_url.or_else(|| bridge.as_ref().map(cli2key::Bridge::base_url));
    prepare::apply(
        &mut command,
        &config.agent,
        &config.provider,
        &config.model,
        codex_executable.as_deref(),
        if config.provider.eq_ignore_ascii_case("cli2key") {
            Some((
                bridge
                    .as_ref()
                    .map_or("asb-replay", cli2key::Bridge::api_key),
                cli2key_endpoint.unwrap_or("http://127.0.0.1:0/v1"),
            ))
        } else {
            None
        },
        (config.agent.eq_ignore_ascii_case("pi")
            && config.provider.eq_ignore_ascii_case("cli2key"))
        .then(|| project::resolve(&project_root, &project_config.tools_dir).join("pi/config"))
        .as_deref(),
    )?;
    if let Some(url) = tape_url {
        tape::configure_command(
            &mut command,
            &config.provider,
            url,
            tape_mode.expect("tape URL implies tape mode"),
        );
    }
    let configured_measurements = config
        .measurements
        .as_ref()
        .filter(|value| value.enabled)
        .map(|value| project::resolve(&project_root, &value.directory));
    let measurement_path = measurement_dir
        .map(std::path::PathBuf::from)
        .or(configured_measurements);
    let measurement_collectors = config
        .measurements
        .as_ref()
        .filter(|value| value.enabled)
        .map_or(&[][..], |value| value.collectors.as_slice());
    let mut measurement_session = measurement_path
        .as_deref()
        .map(|dir| {
            measurements::Session::start_with_collectors(
                dir,
                measurement_collectors,
                Some(&project::resolve(&project_root, &project_config.tools_dir)),
            )
        })
        .transpose()
        .map_err(Error::Run)?;
    let child = command
        .spawn()
        .map_err(|error| Error::Run(format!("could not start {program}: {error}")))?;
    if let Some(session) = measurement_session.as_mut() {
        session.attach_pid(child.id()).map_err(Error::Run)?;
    }
    let child = child
        .wait_with_output()
        .map_err(|error| Error::Run(format!("could not wait for {program}: {error}")))?;
    if let Some(session) = tape_session.take() {
        session.stop()?;
    }
    if let Some((tape::Mode::Record, cassette)) = tape_cassette.as_ref() {
        let metadata = std::fs::metadata(cassette).map_err(|error| {
            Error::Run(format!(
                "record completed without creating cassette {}; verify the provider endpoint and llmtape installation: {error}",
                cassette.display()
            ))
        })?;
        if metadata.len() == 0 {
            return Err(Error::Run(format!(
                "record created an empty cassette {}; the agent made no provider request; verify its provider base URL and credentials",
                cassette.display()
            )));
        }
    }
    let elapsed_ms = started.elapsed().as_millis();
    let success = child.status.success();
    let token_usage = measurements::token_usage_from_sources(
        &config.provider,
        &config.agent,
        &bounded(&child.stdout),
        &bounded(&child.stderr),
        tape_cassette
            .as_ref()
            .map(|(_, cassette)| cassette.as_path()),
    );
    let result = RunResult {
        status: if success { "completed" } else { "failed" },
        agent: &config.agent,
        workload: &config.workload,
        provider: &config.provider,
        model: &config.model,
        success_rate: if success { 1.0 } else { 0.0 },
        elapsed_ms,
        exit_code: child.status.code(),
        token_usage,
        cassette: tape_cassette
            .as_ref()
            .map(|(_, cassette)| cassette.display().to_string()),
        stdout: bounded(&child.stdout),
        stderr: bounded(&child.stderr),
    };
    write_log(config_path, &config.log_file, &result)?;
    if let Some(session) = measurement_session {
        let measured = measurements::RunMeasurement {
            schema_version: 1,
            run_id: format!("{}-{}", result.workload, result.elapsed_ms),
            status: result.status,
            success,
            success_rate: result.success_rate,
            agent: result.agent,
            workload: result.workload,
            provider: result.provider,
            model: result.model,
            elapsed_ms,
            exit_code: result.exit_code,
            token_usage: result.token_usage.clone(),
            started_unix_ms: session.started_unix_ms(),
        };
        session.finish(measured).map_err(Error::Run)?;
    }
    if json {
        render_json(&result, output)?;
    } else {
        render_human(&result, output)?;
    }
    if success {
        Ok(())
    } else {
        Err(Error::Run("agent exited unsuccessfully".into()))
    }
}

fn default_cassette(config_path: &Path, config: &BenchmarkConfig) -> std::path::PathBuf {
    let stem = config_path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("benchmark");
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("cassettes")
        .join(format!(
            "{stem}-{}-{}-{}.jsonl",
            safe_name(&config.agent),
            safe_name(&config.workload),
            safe_name(&config.model)
        ))
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn write_log(
    config_path: &Path,
    configured_path: &str,
    result: &RunResult<'_>,
) -> Result<(), Error> {
    let path = resolve_log_path(config_path, configured_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::Run(format!(
                "cannot create log directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| Error::Run(format!("cannot open log file {}: {error}", path.display())))?;
    serde_json::to_writer(&mut file, result).map_err(|error| {
        Error::Run(format!("cannot write log file {}: {error}", path.display()))
    })?;
    writeln!(file).map_err(|error| {
        Error::Run(format!(
            "cannot finish log file {}: {error}",
            path.display()
        ))
    })
}

fn resolve_log_path(config_path: &Path, configured_path: &str) -> PathBuf {
    let path = Path::new(configured_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
    }
}

fn render_json(result: &RunResult<'_>, output: &mut dyn Write) -> Result<(), Error> {
    let summary = serde_json::json!({
        "success_rate": result.success_rate,
        "elapsed_ms": result.elapsed_ms,
        "token_usage": result.token_usage,
    });
    serde_json::to_writer_pretty(&mut *output, &summary)
        .map_err(|error| Error::Io(error.to_string()))?;
    writeln!(output).map_err(io_error)
}

fn render_human(result: &RunResult<'_>, output: &mut dyn Write) -> Result<(), Error> {
    writeln!(
        output,
        "success_rate={:.3} elapsed_ms={} input_tokens={} output_tokens={} cached_tokens={} total_tokens={}",
        result.success_rate,
        result.elapsed_ms,
        format_token(result.token_usage.input_tokens),
        format_token(result.token_usage.output_tokens),
        format_token(result.token_usage.cached_tokens),
        format_token(result.token_usage.total_tokens),
    )
    .map_err(io_error)
}

fn format_token(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".into(), |value| value.to_string())
}

struct CommandSpec {
    program: String,
    args: Vec<String>,
    append_prompt: bool,
}

fn command_for(
    config: &BenchmarkConfig,
    cli2key_base_url: Option<&str>,
    prompt: &str,
) -> Result<CommandSpec, Error> {
    if let Some(candidate) = suggestions::closest(&config.agent, BUILTIN_AGENTS) {
        return Err(Error::Input(format!(
            "unknown agent `{}`; did you mean `{candidate}`?",
            config.agent
        )));
    }
    if let Some(candidates) = providers::known_models(&config.provider) {
        if let Some(candidate) = suggestions::closest(&config.model, candidates) {
            return Err(Error::Input(format!(
                "unknown model `{}` for provider `{}`; did you mean `{candidate}`?",
                config.model, config.provider
            )));
        }
    }
    let spec = match config.agent.as_str() {
        "codex" => CommandSpec {
            program: "codex".into(),
            args: vec![
                "exec".into(),
                "--skip-git-repo-check".into(),
                "--model".into(),
                config.model.clone(),
            ],
            append_prompt: true,
        },
        "pi" => CommandSpec {
            program: "pi".into(),
            args: vec![
                "--provider".into(),
                pi_provider(config),
                "--model".into(),
                config.model.clone(),
                "--print".into(),
            ],
            append_prompt: true,
        },
        "aider" => CommandSpec {
            program: "aider".into(),
            args: vec!["--message".into()],
            append_prompt: true,
        },
        "opencode" => CommandSpec {
            program: "opencode".into(),
            args: vec![
                "run".into(),
                "--model".into(),
                opencode_model(config),
                "--format".into(),
                "json".into(),
            ],
            append_prompt: true,
        },
        "qwen-code" => CommandSpec {
            program: "qwen".into(),
            args: vec!["--model".into(), config.model.clone(), "--yolo".into()],
            append_prompt: true,
        },
        "mini-swe" => CommandSpec {
            program: "mini".into(),
            args: vec![
                "--model".into(),
                format!("anthropic/{}", config.model),
                "--task".into(),
                prompt.into(),
                "--yolo".into(),
                "--exit-immediately".into(),
            ],
            append_prompt: false,
        },
        "openhands" => CommandSpec {
            program: "openhands".into(),
            args: vec![
                "--headless".into(),
                "--yolo".into(),
                "--override-with-envs".into(),
                "--task".into(),
                prompt.into(),
            ],
            append_prompt: false,
        },
        "openjiuwen" => CommandSpec {
            program: "openjiuwen".into(),
            args: [
                "--model",
                config.model.as_str(),
                "--provider",
                "anthropic",
                "--api-base",
                cli2key_base_url.unwrap_or_default(),
                "run",
                prompt,
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            append_prompt: false,
        },
        "gemini" => CommandSpec {
            program: "gemini".into(),
            args: vec![
                "-p".into(),
                prompt.into(),
                "-m".into(),
                config.model.clone(),
                "--yolo".into(),
            ],
            append_prompt: false,
        },
        "workbuddy" => CommandSpec {
            program: "codebuddy".into(),
            args: vec!["--print".into(), "--model".into(), config.model.clone(), "-y".into()],
            append_prompt: true,
        },
        "codeagent" => {
            return Err(Error::Run(
                "codeagent provides a web server and has no non-interactive prompt command; use an executable agent such as codex, opencode, qwen-code, openhands, or openjiuwen".into(),
            ))
        }
        other => CommandSpec {
            program: other.into(),
            args: Vec::new(),
            append_prompt: true,
        },
    };
    Ok(spec)
}

fn opencode_model(config: &BenchmarkConfig) -> String {
    if config.model.contains('/') {
        return config.model.clone();
    }
    if config.provider.eq_ignore_ascii_case("cli2key") {
        format!("cli2key/{}", config.model)
    } else if config.provider.eq_ignore_ascii_case("opencode-zen") {
        format!("opencode/{}", config.model)
    } else {
        config.model.clone()
    }
}

fn pi_provider(config: &BenchmarkConfig) -> String {
    config.provider.clone()
}

fn local_agent(agent: &str, project_root: &Path, tools_dir: &str) -> Option<String> {
    let executable = match agent {
        "qwen-code" => "qwen",
        "mini-swe" => "mini",
        "cli2key" => "codex-bridge",
        "codeagent" => "code-agent",
        "workbuddy" => "codebuddy",
        other => other,
    };
    let mut candidates = Vec::new();
    if let Some(directory) = std::env::var_os("ASB_TOOL_DIR") {
        candidates.push(std::path::PathBuf::from(directory).join(executable));
    }
    candidates.push(
        project::resolve(project_root, tools_dir)
            .join(agent)
            .join("bin")
            .join(executable),
    );
    candidates.push(
        project::resolve(project_root, tools_dir)
            .join(agent)
            .join("node_modules/.bin")
            .join(executable),
    );
    candidates.push(
        project::resolve(project_root, tools_dir)
            .join(agent)
            .join("venv/bin")
            .join(executable),
    );
    candidates.push(Path::new(".asb-tools/bin").join(executable));
    if executable != agent {
        candidates.push(
            project::resolve(project_root, tools_dir)
                .join(agent)
                .join("bin")
                .join(agent),
        );
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| candidate.canonicalize().ok())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

fn bounded(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_OUTPUT)]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        RunResult, command_for, opencode_model, render_human, render_json, resolve_log_path,
        write_log,
    };
    use crate::benchmark::BenchmarkConfig;
    use crate::measurements::TokenUsage;

    fn result<'a>() -> RunResult<'a> {
        RunResult {
            status: "completed",
            agent: "codex",
            workload: "bug-fix",
            provider: "openrouter",
            model: "test-model",
            success_rate: 1.0,
            elapsed_ms: 12,
            exit_code: Some(0),
            token_usage: TokenUsage {
                input_tokens: Some(3),
                output_tokens: Some(4),
                cached_tokens: Some(1),
                total_tokens: Some(7),
                source: Some("test".into()),
            },
            cassette: None,
            stdout: "done".into(),
            stderr: String::new(),
        }
    }

    #[test]
    fn human_output_is_concise_and_not_json() {
        let mut output = Vec::new();
        render_human(&result(), &mut output).expect("human rendering succeeds");
        let text = String::from_utf8(output).expect("utf8");
        assert!(text.starts_with("success_rate=1.000"));
        assert!(text.contains("elapsed_ms=12"));
        assert!(text.contains("cached_tokens=1"));
        assert!(text.contains("total_tokens=7"));
        assert!(!text.contains("done"));
        assert!(!text.starts_with('{'));
    }

    #[test]
    fn json_output_remains_machine_readable() {
        let mut output = Vec::new();
        render_json(&result(), &mut output).expect("json rendering succeeds");
        let value: serde_json::Value = serde_json::from_slice(&output).expect("valid json");
        assert_eq!(value["success_rate"], 1.0);
        assert_eq!(value["elapsed_ms"], 12);
        assert_eq!(value["token_usage"]["total_tokens"], 7);
        assert_eq!(value["token_usage"]["cached_tokens"], 1);
        assert!(value.get("stdout").is_none());
    }

    #[test]
    fn log_path_is_relative_to_configuration_file() {
        assert_eq!(
            resolve_log_path(std::path::Path::new("projects/foo.toml"), "foo.log"),
            std::path::PathBuf::from("projects/foo.log")
        );
        assert_eq!(
            resolve_log_path(
                std::path::Path::new("projects/foo.toml"),
                "/tmp/asb-run.log"
            ),
            std::path::PathBuf::from("/tmp/asb-run.log")
        );
    }

    #[test]
    fn log_contains_full_bounded_result_while_summary_hides_agent_output() {
        let directory = std::env::temp_dir().join(format!("asb-runner-log-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("log test directory");
        let config = directory.join("foo.toml");
        write_log(&config, "foo.log", &result()).expect("log writes");
        let text = std::fs::read_to_string(directory.join("foo.log")).expect("log reads");
        let value: serde_json::Value = serde_json::from_str(text.trim()).expect("log JSON");
        assert_eq!(value["stdout"], "done");
        assert_eq!(value["success_rate"], 1.0);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn cli2key_command_mappings_place_workload_once() {
        let config = BenchmarkConfig {
            agent: "openhands".into(),
            workload: "bug-fix".into(),
            provider: "cli2key".into(),
            model: "gpt-test".into(),
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
        };
        let command = command_for(&config, Some("http://127.0.0.1:3456/v1"), "prompt")
            .expect("known agent has a command");
        assert_eq!(command.program, "openhands");
        assert!(!command.append_prompt);
        assert_eq!(command.args.last().map(String::as_str), Some("prompt"));
    }

    #[test]
    fn pi_cli2key_uses_openai_bridge_and_print_mode() {
        let config = BenchmarkConfig {
            agent: "pi".into(),
            workload: "bug-fix".into(),
            provider: "cli2key".into(),
            model: "gpt-6.1-sol".into(),
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
        };
        let command = command_for(&config, Some("http://127.0.0.1:3456/v1"), "prompt")
            .expect("pi has a command");
        assert_eq!(command.program, "pi");
        assert_eq!(command.args[0], "--provider");
        assert_eq!(command.args[1], "cli2key");
        assert!(command.args.contains(&"--print".into()));
        assert!(command.append_prompt);
    }

    #[test]
    fn opencode_zen_uses_opencode_provider_model_namespace() {
        let config = BenchmarkConfig {
            agent: "opencode".into(),
            workload: "bug-fix".into(),
            provider: "opencode-zen".into(),
            model: "mimo-v2.6-flash-free".into(),
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
        };
        assert_eq!(opencode_model(&config), "opencode/mimo-v2.6-flash-free");
    }

    #[test]
    fn codeagent_is_rejected_without_a_noninteractive_command() {
        let config = BenchmarkConfig {
            agent: "codeagent".into(),
            workload: "bug-fix".into(),
            provider: "cli2key".into(),
            model: "gpt-test".into(),
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
        };
        assert!(command_for(&config, Some("http://127.0.0.1:3456/v1"), "prompt").is_err());
    }

    #[test]
    fn unknown_agent_suggests_a_close_match() {
        let config = BenchmarkConfig {
            agent: "opencod".into(),
            workload: "bug-fix".into(),
            provider: "local".into(),
            model: "test-model".into(),
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
        };
        let error = match command_for(&config, None, "prompt") {
            Ok(_) => panic!("typo rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("did you mean `opencode`?"));
    }
}
