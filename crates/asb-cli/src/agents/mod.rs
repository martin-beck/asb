//! Extensible adapters for benchmark agents.
//!
//! Generic runner code owns process lifecycle, output capture, measurements,
//! and provider wiring. Each adapter owns its stable identity, executable and
//! non-interactive command shape; adding an agent is therefore a new module
//! and registry entry instead of another runner branch.

mod claude_code;
mod cline;
mod codex;
mod continue_agent;
mod goose;
mod mini_swe;
mod opencode;
mod openhands;
mod openjiuwen;
mod pi;
mod qwen_code;
mod workbuddy;

use crate::benchmark::BenchmarkConfig;
use crate::project;
use crate::{Error, prepare, providers};
use std::path::Path;
use std::process::Command;

pub(crate) trait AgentAdapter: Sync {
    fn id(&self) -> &'static str;
    fn executable(&self) -> &'static str;
    fn command(&self, context: &CommandContext<'_>) -> CommandSpec;
    fn prepare(&self, context: &mut PreparationContext<'_>) -> Result<(), Error>;

    /// Hook for adapter-owned result or artifact extraction. The generic runner
    /// calls this after output is captured; built-ins currently need no extra
    /// artifact extraction but custom adapters can opt in without runner edits.
    fn collect(&self, _output: &str) -> AgentArtifacts {
        AgentArtifacts::default()
    }
}

/// Adapter-owned details that remain after provider-wide preparation.
pub(crate) struct PreparationContext<'a> {
    pub command: &'a mut Command,
    pub provider: &'a str,
    pub model: &'a str,
    pub cli2key: Option<(&'a str, &'a str)>,
    pub codex_config_dir: Option<&'a Path>,
    pub pi_config_dir: Option<&'a Path>,
    pub workbuddy_config_dir: Option<&'a Path>,
    pub continue_config_dir: Option<&'a Path>,
    pub cline_data_dir: Option<&'a Path>,
}

pub(crate) struct CommandContext<'a> {
    pub config: &'a BenchmarkConfig,
    pub endpoint: Option<&'a str>,
    pub prompt: &'a str,
}

pub(crate) struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub append_prompt: bool,
}

#[derive(Default)]
pub(crate) struct AgentArtifacts {
    pub paths: Vec<String>,
}

#[derive(Clone, Copy)]
pub(super) enum CommandKind {
    Codex,
    Pi,
    Opencode,
    Workbuddy,
    QwenCode,
    MiniSwe,
    Openhands,
    Openjiuwen,
    ClaudeCode,
    Goose,
    Cline,
    Continue,
}

pub(super) struct BuiltinAdapter {
    id: &'static str,
    executable: &'static str,
    kind: CommandKind,
}

impl BuiltinAdapter {
    pub(super) const fn new(id: &'static str, executable: &'static str, kind: CommandKind) -> Self {
        Self {
            id,
            executable,
            kind,
        }
    }
}

impl AgentAdapter for BuiltinAdapter {
    fn id(&self) -> &'static str {
        self.id
    }
    fn executable(&self) -> &'static str {
        self.executable
    }

    fn command(&self, context: &CommandContext<'_>) -> CommandSpec {
        let config = context.config;
        match self.kind {
            CommandKind::Codex => CommandSpec {
                program: self.executable.into(),
                args: vec![
                    "exec".into(),
                    "--skip-git-repo-check".into(),
                    "--ephemeral".into(),
                    "--ignore-rules".into(),
                    "--json".into(),
                    "--model".into(),
                    config.model.clone(),
                ],
                append_prompt: true,
            },
            CommandKind::Pi => CommandSpec {
                program: self.executable.into(),
                args: vec![
                    "--provider".into(),
                    config.provider.clone(),
                    "--model".into(),
                    config.model.clone(),
                    "--mode".into(),
                    "json".into(),
                    "--print".into(),
                ],
                append_prompt: true,
            },
            CommandKind::Opencode => CommandSpec {
                program: self.executable.into(),
                args: vec![
                    "run".into(),
                    "--model".into(),
                    opencode_model(config),
                    "--format".into(),
                    "json".into(),
                ],
                append_prompt: true,
            },
            CommandKind::Workbuddy => CommandSpec {
                program: self.executable.into(),
                args: vec![
                    "--print".into(),
                    "--output-format".into(),
                    "json".into(),
                    "--model".into(),
                    config.model.clone(),
                    "-y".into(),
                ],
                append_prompt: true,
            },
            CommandKind::QwenCode => CommandSpec {
                program: self.executable.into(),
                args: vec![
                    "--model".into(),
                    config.model.clone(),
                    "--output-format".into(),
                    "json".into(),
                    "--yolo".into(),
                ],
                append_prompt: true,
            },
            CommandKind::MiniSwe => CommandSpec {
                program: self.executable.into(),
                args: vec![
                    "--model".into(),
                    format!("anthropic/{}", config.model),
                    "--task".into(),
                    context.prompt.into(),
                    "--yolo".into(),
                    "--exit-immediately".into(),
                    "--output".into(),
                    ".asb-mini-swe.traj.json".into(),
                ],
                append_prompt: false,
            },
            CommandKind::Openhands => CommandSpec {
                program: self.executable.into(),
                args: vec![
                    "--headless".into(),
                    "--json".into(),
                    "--yolo".into(),
                    "--override-with-envs".into(),
                    "--task".into(),
                    context.prompt.into(),
                ],
                append_prompt: false,
            },
            CommandKind::Openjiuwen => CommandSpec {
                program: self.executable.into(),
                args: [
                    "--model",
                    config.model.as_str(),
                    "--provider",
                    "anthropic",
                    "--api-base",
                    context.endpoint.unwrap_or_default(),
                    "run",
                    "-f",
                    "json",
                    context.prompt,
                ]
                .into_iter()
                .map(str::to_owned)
                .collect(),
                append_prompt: false,
            },
            CommandKind::ClaudeCode => CommandSpec {
                program: self.executable.into(),
                args: vec![
                    "-p".into(),
                    "--bare".into(),
                    "--no-session-persistence".into(),
                    "--output-format".into(),
                    "json".into(),
                    "--model".into(),
                    config.model.clone(),
                    "--dangerously-skip-permissions".into(),
                    context.prompt.into(),
                ],
                append_prompt: false,
            },
            CommandKind::Goose => CommandSpec {
                program: self.executable.into(),
                args: vec![
                    "run".into(),
                    "--no-session".into(),
                    "--output-format".into(),
                    "json".into(),
                    "-t".into(),
                    context.prompt.into(),
                ],
                append_prompt: false,
            },
            CommandKind::Cline => CommandSpec {
                program: self.executable.into(),
                args: vec![
                    "--json".into(),
                    "--auto-approve".into(),
                    "true".into(),
                    "--provider".into(),
                    if config.provider.eq_ignore_ascii_case("openrouter") {
                        "openrouter".into()
                    } else {
                        "openai-native".into()
                    },
                    "--model".into(),
                    config.model.clone(),
                    context.prompt.into(),
                ],
                append_prompt: false,
            },
            CommandKind::Continue => CommandSpec {
                program: self.executable.into(),
                args: vec![
                    "-p".into(),
                    context.prompt.into(),
                    "--model".into(),
                    config.model.clone(),
                    "--format".into(),
                    "json".into(),
                    "--auto".into(),
                ],
                append_prompt: false,
            },
        }
    }

    fn prepare(&self, context: &mut PreparationContext<'_>) -> Result<(), Error> {
        let is_cli2key = context.provider.eq_ignore_ascii_case("cli2key");
        let is_openrouter = context.provider.eq_ignore_ascii_case("openrouter");
        let credentials = context.cli2key;
        match self.kind {
            CommandKind::Codex if is_cli2key => {
                let directory = context.codex_config_dir.ok_or_else(|| {
                    Error::Run(
                        "cannot configure Codex without a project-local configuration directory"
                            .into(),
                    )
                })?;
                let (_, base_url) = credentials
                    .ok_or_else(|| Error::Run("cli2key provider has no API endpoint".into()))?;
                prepare::configure_codex(directory, context.model, base_url)?;
                context.command.env("CODEX_HOME", directory);
                Ok(())
            }
            CommandKind::Pi if is_cli2key || is_openrouter => {
                let directory = context.pi_config_dir.ok_or_else(|| Error::Run(
                    "cannot configure Pi without a project-local provider configuration directory".into(),
                ))?;
                let (provider, api, key_name) = if is_cli2key {
                    ("cli2key", "openai-responses", "CLI2KEY_API_KEY")
                } else {
                    ("openrouter", "openai-completions", "OPENROUTER_API_KEY")
                };
                let base_url = if let Some((_, base_url)) = credentials {
                    base_url.to_owned()
                } else {
                    std::env::var("OPENROUTER_BASE_URL")
                        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into())
                };
                prepare::configure_pi_provider(
                    context.command,
                    directory,
                    provider,
                    context.model,
                    &base_url,
                    api,
                    key_name,
                )
            }
            CommandKind::Opencode if is_cli2key => {
                let (_, base_url) = credentials
                    .ok_or_else(|| Error::Run("cli2key provider has no API endpoint".into()))?;
                context.command.env(
                    "OPENCODE_CONFIG_CONTENT",
                    prepare::opencode_config(context.model, base_url)?,
                );
                Ok(())
            }
            CommandKind::QwenCode if is_cli2key => {
                context
                    .command
                    .env_remove("OPENAI_API_KEY")
                    .env_remove("OPENAI_BASE_URL")
                    .env_remove("OPENAI_API_BASE")
                    .env_remove("OPENAI_MODEL");
                Ok(())
            }
            CommandKind::MiniSwe if is_cli2key => {
                context
                    .command
                    .env("MSWEA_CONFIGURED", "true")
                    .env("MSWEA_MODEL_NAME", format!("anthropic/{}", context.model))
                    .env("MSWEA_SILENT_STARTUP", "1")
                    .env("MSWEA_COST_TRACKING", "ignore_errors");
                Ok(())
            }
            CommandKind::Openhands if is_cli2key => {
                let (key, base_url) = credentials
                    .ok_or_else(|| Error::Run("cli2key provider has no API endpoint".into()))?;
                context
                    .command
                    .env("LLM_API_KEY", key)
                    .env("LLM_BASE_URL", base_url.trim_end_matches("/v1"))
                    .env("LLM_MODEL", format!("anthropic/{}", context.model))
                    .env("OPENHANDS_SUPPRESS_BANNER", "1");
                Ok(())
            }
            CommandKind::Workbuddy if is_cli2key => {
                let (key, base_url) = credentials
                    .ok_or_else(|| Error::Run("cli2key provider has no API endpoint".into()))?;
                let directory = context.workbuddy_config_dir.ok_or_else(|| Error::Run("cannot configure WorkBuddy without a project-local configuration directory".into()))?;
                prepare::configure_workbuddy(directory, context.model, base_url)?;
                context
                    .command
                    .env("CODEBUDDY_API_KEY", key)
                    .env("CODEBUDDY_AUTH_TOKEN", key)
                    .env("CODEBUDDY_BASE_URL", base_url)
                    .env("CODEBUDDY_MODEL", context.model)
                    .env("CODEBUDDY_CONFIG_DIR", directory)
                    .env("CODEBUDDY_API_KEY_HELPER_DISABLED", "1");
                Ok(())
            }
            CommandKind::Openjiuwen if is_cli2key => {
                if let Some((key, _)) = credentials {
                    context.command.env("OPENJIUWEN_API_KEY", key);
                }
                Ok(())
            }
            CommandKind::Goose if is_cli2key => {
                if let Some((key, base_url)) = credentials {
                    context
                        .command
                        .env("GOOSE_PROVIDER", "anthropic")
                        .env("GOOSE_MODEL", context.model)
                        .env("ANTHROPIC_HOST", base_url.trim_end_matches("/v1"))
                        .env("ANTHROPIC_API_KEY", key)
                        .env("ANTHROPIC_AUTH_TOKEN", key);
                }
                Ok(())
            }
            CommandKind::ClaudeCode if is_cli2key => {
                let (key, base_url) = credentials
                    .ok_or_else(|| Error::Run("cli2key provider has no API endpoint".into()))?;
                context
                    .command
                    .env("ANTHROPIC_BASE_URL", base_url)
                    .env("ANTHROPIC_API_KEY", key)
                    .env("ANTHROPIC_AUTH_TOKEN", key)
                    .env_remove("CLAUDE_CODE_USE_BEDROCK")
                    .env_remove("CLAUDE_CODE_USE_VERTEX");
                Ok(())
            }
            CommandKind::Cline if is_cli2key => {
                if let Some((key, base_url)) = credentials {
                    let directory = context.cline_data_dir.ok_or_else(|| Error::Run(
                        "cannot configure Cline without a project-local provider configuration directory".into(),
                    ))?;
                    prepare::configure_cline(
                        directory,
                        "openai-native",
                        context.model,
                        base_url,
                        key,
                    )?;
                    context
                        .command
                        .env("OPENAI_API_KEY", key)
                        .env("OPENAI_BASE_URL", base_url)
                        .env("OPENAI_API_BASE", base_url)
                        .env("OPENAI_MODEL", context.model)
                        .env("CLINE_API_KEY", key)
                        .env("CLINE_DIR", directory)
                        .env("CLINE_DATA_DIR", directory.join("data"))
                        .env(
                            "CLINE_PROVIDER_SETTINGS_PATH",
                            directory.join("data/settings/providers.json"),
                        )
                        .env(
                            "CLINE_GLOBAL_SETTINGS_PATH",
                            directory.join("data/settings/global-settings.json"),
                        );
                }
                Ok(())
            }
            CommandKind::Cline if is_openrouter => {
                let directory = context.cline_data_dir.ok_or_else(|| Error::Run(
                    "cannot configure Cline without a project-local provider configuration directory".into(),
                ))?;
                let base_url = std::env::var("OPENROUTER_BASE_URL")
                    .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into());
                let key = providers::credential("openrouter").ok_or_else(|| {
                    Error::Run(
                        "openrouter provider requires OPENROUTER_API_KEY or ~/.api_key_openrouter"
                            .into(),
                    )
                })?;
                prepare::configure_cline(directory, "openrouter", context.model, &base_url, &key)?;
                context
                    .command
                    .env("CLINE_DIR", directory)
                    .env("CLINE_DATA_DIR", directory.join("data"))
                    .env(
                        "CLINE_PROVIDER_SETTINGS_PATH",
                        directory.join("data/settings/providers.json"),
                    )
                    .env(
                        "CLINE_GLOBAL_SETTINGS_PATH",
                        directory.join("data/settings/global-settings.json"),
                    );
                Ok(())
            }
            CommandKind::Continue if is_cli2key || is_openrouter => {
                let directory = context.continue_config_dir.ok_or_else(|| {
                    Error::Run(
                        "cannot configure Continue without a project-local configuration directory"
                            .into(),
                    )
                })?;
                let base_url = credentials
                    .map(|(_, url)| url.to_owned())
                    .or_else(|| std::env::var("OPENROUTER_BASE_URL").ok())
                    .unwrap_or_else(|| "https://openrouter.ai/api/v1".into());
                let key_name = if is_cli2key {
                    "CLI2KEY_API_KEY"
                } else {
                    "OPENROUTER_API_KEY"
                };
                let continue_provider = if is_cli2key { "anthropic" } else { "openai" };
                prepare::configure_continue(
                    directory,
                    continue_provider,
                    context.model,
                    &base_url,
                    key_name,
                )?;
                context
                    .command
                    .env("CONTINUE_CONFIG", directory.join("config.yaml"))
                    .env("CONTINUE_GLOBAL_DIR", directory);
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

pub(crate) fn prepare(
    command: &mut Command,
    agent: &str,
    provider: &str,
    model: &str,
    cli2key: Option<(&str, &str)>,
    codex_config_dir: Option<&Path>,
    pi_config_dir: Option<&Path>,
    workbuddy_config_dir: Option<&Path>,
    continue_config_dir: Option<&Path>,
    cline_data_dir: Option<&Path>,
) -> Result<(), Error> {
    let Some(adapter) = find(agent) else {
        return Ok(());
    };
    adapter.prepare(&mut PreparationContext {
        command,
        provider,
        model,
        cli2key,
        codex_config_dir,
        pi_config_dir,
        workbuddy_config_dir,
        continue_config_dir,
        cline_data_dir,
    })
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

static REGISTRY: &[&dyn AgentAdapter] = &[
    &codex::ADAPTER,
    &pi::ADAPTER,
    &opencode::ADAPTER,
    &workbuddy::ADAPTER,
    &qwen_code::ADAPTER,
    &mini_swe::ADAPTER,
    &openhands::ADAPTER,
    &openjiuwen::ADAPTER,
    &claude_code::ADAPTER,
    &goose::ADAPTER,
    &cline::ADAPTER,
    &continue_agent::ADAPTER,
];

pub(crate) fn all() -> &'static [&'static dyn AgentAdapter] {
    REGISTRY
}
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    REGISTRY.iter().map(|adapter| adapter.id())
}
pub(crate) fn find(id: &str) -> Option<&'static dyn AgentAdapter> {
    REGISTRY.iter().copied().find(|adapter| adapter.id() == id)
}

pub(crate) fn resolve_executable(
    id: &str,
    executable: &str,
    project_root: &Path,
    tools_dir: &str,
) -> Option<String> {
    let mut candidates = Vec::new();
    if let Some(directory) = std::env::var_os("ASB_TOOL_DIR") {
        candidates.push(std::path::PathBuf::from(directory).join(executable));
    }
    let root = project::resolve(project_root, tools_dir).join(id);
    candidates.extend([
        root.join("bin").join(executable),
        root.join("node_modules/.bin").join(executable),
        root.join("venv/bin").join(executable),
        Path::new(".asb-tools/bin").join(executable),
    ]);
    candidates
        .into_iter()
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| candidate.canonicalize().ok())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

pub(crate) fn resolve(id: &str, project_root: &Path, tools_dir: &str) -> Option<String> {
    let adapter = find(id)?;
    resolve_executable(adapter.id(), adapter.executable(), project_root, tools_dir)
}
