//! Local llmtape proxy lifecycle for deterministic LLM recording and replay.

use crate::Error;
use crate::providers;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Record,
    Replay,
}

/// A running local llmtape server. Dropping it always attempts to stop it.
pub struct Session {
    child: Child,
    url: String,
}

impl Session {
    /// Start llmtape on an available loopback port and wait until it accepts connections.
    pub fn start(
        mode: Mode,
        cassette: &Path,
        upstream: Option<&str>,
        project_executable: Option<&Path>,
    ) -> Result<Self, Error> {
        if mode == Mode::Replay && !cassette.is_file() {
            return Err(Error::Input(format!(
                "replay cassette {} does not exist; run `asb run CONFIG --record --cassette {}` first",
                cassette.display(),
                cassette.display()
            )));
        }
        if mode == Mode::Record {
            let parent = cassette.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::Run(format!(
                    "cannot create cassette directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let port = free_port()?;
        let executable = resolve_executable(project_executable)?;
        let mut command = Command::new(&executable);
        command
            .arg(match mode {
                Mode::Record => "record",
                Mode::Replay => "replay",
            })
            .arg("--port")
            .arg(port.to_string());
        match mode {
            Mode::Record => {
                let upstream = upstream.ok_or_else(|| {
                    Error::Run(
                        "record requires a provider API endpoint; set the provider base URL or use cli2key".into(),
                    )
                })?;
                command.args(["--upstream", upstream, "--out"]);
                command.arg(cassette);
            }
            Mode::Replay => {
                command.args(["--cassette"]);
                command.arg(cassette);
            }
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().map_err(|error| {
            Error::Run(format!(
                "cannot start project llmtape (`{executable}`); run `asb tool install llmtape` in the benchmark project or set ASB_LLM_TAPE to another executable: {error}"
            ))
        })?;
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
                return Ok(Self {
                    child,
                    url: format!("http://127.0.0.1:{port}/v1"),
                });
            }
            thread::sleep(Duration::from_millis(50));
        }
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        Err(Error::Run(
            "llmtape did not become ready within 8 seconds; verify the llmtape installation and cassette format".into(),
        ))
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn stop(mut self) -> Result<(), Error> {
        let _ = self.child.kill();
        self.child
            .wait()
            .map(|_| ())
            .map_err(|error| Error::Run(format!("could not stop llmtape: {error}")))
    }
}

/// Resolve llmtape without silently depending on a host-wide installation.
///
/// The runner passes the executable found in the benchmark project's tool
/// directory.  `ASB_LLM_TAPE` remains an explicit escape hatch for a caller
/// that deliberately supplies another executable; an unqualified `llmtape`
/// lookup on `PATH` is intentionally not performed.
fn resolve_executable(project_executable: Option<&Path>) -> Result<String, Error> {
    resolve_executable_with_override(project_executable, std::env::var("ASB_LLM_TAPE").ok())
}

fn resolve_executable_with_override(
    project_executable: Option<&Path>,
    explicit_override: Option<String>,
) -> Result<String, Error> {
    project_executable
        .map(|path| path.to_string_lossy().into_owned())
        .or(explicit_override)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            Error::Run(
                "llmtape is not installed in this benchmark project; run `asb tool install llmtape` from the project directory, or set ASB_LLM_TAPE to an explicit executable".into(),
            )
        })
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Point common provider clients at the local tape proxy.
pub fn configure_command(command: &mut Command, provider: &str, url: &str, mode: Mode) {
    let anthropic_url = url.trim_end_matches("/v1");
    for name in [
        "OPENAI_BASE_URL",
        "OPENAI_API_BASE",
        "OPENROUTER_BASE_URL",
        "GROQ_BASE_URL",
        "MISTRAL_BASE_URL",
        "DEEPSEEK_BASE_URL",
        "XAI_BASE_URL",
        "TOGETHER_BASE_URL",
        "FIREWORKS_BASE_URL",
        "GOOGLE_GEMINI_BASE_URL",
        "OLLAMA_HOST",
        "CLI2KEY_BASE_URL",
    ] {
        command.env(name, url);
    }
    command
        .env("ANTHROPIC_BASE_URL", anthropic_url)
        .env("LLM_BASE_URL", anthropic_url)
        .env("ASB_PROVIDER_BASE_URL", url)
        .env(
            "ASB_TAPE_MODE",
            match mode {
                Mode::Record => "record",
                Mode::Replay => "replay",
            },
        );
    if provider.eq_ignore_ascii_case("cli2key") {
        command.env("CLI2KEY_BASE_URL", url);
    }
}

/// Resolve the upstream API base used by llmtape in record mode.
pub fn upstream(provider: &str, bridge_base_url: Option<&str>) -> Option<String> {
    if provider.eq_ignore_ascii_case("cli2key") {
        return bridge_base_url.map(strip_api_version);
    }
    if let Some(value) = std::env::var_os("ASB_PROVIDER_BASE_URL") {
        let value = value.to_string_lossy().trim().to_owned();
        if !value.is_empty() {
            return Some(strip_api_version(&value));
        }
    }
    providers::model_spec(provider)
        .and_then(|spec| spec.url())
        .map(|url| {
            ["/models", "/tags"]
                .iter()
                .find_map(|suffix| url.strip_suffix(suffix))
                .unwrap_or(&url)
                .to_owned()
        })
        .map(|url| strip_api_version(&url))
}

fn strip_api_version(value: &str) -> String {
    value
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_owned()
}

fn free_port() -> Result<u16, Error> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map(|listener| listener.local_addr().map(|address| address.port()))
        .map_err(|error| Error::Run(format!("cannot reserve a local llmtape port: {error}")))?
        .map_err(|error| Error::Run(format!("cannot inspect local llmtape port: {error}")))
}

#[cfg(test)]
mod tests {
    use super::{
        Mode, configure_command, resolve_executable, resolve_executable_with_override, upstream,
    };
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn upstream_uses_bridge_for_cli2key_and_known_provider_defaults() {
        assert_eq!(
            upstream("cli2key", Some("http://127.0.0.1:3456/v1")).as_deref(),
            Some("http://127.0.0.1:3456")
        );
        assert_eq!(
            upstream("openrouter", None).as_deref(),
            Some("https://openrouter.ai/api")
        );
    }

    #[test]
    fn endpoint_configuration_is_local_and_mode_explicit() {
        let mut command = Command::new("agent");
        configure_command(
            &mut command,
            "openrouter",
            "http://127.0.0.1:3456/v1",
            Mode::Replay,
        );
        let values = command
            .get_envs()
            .filter_map(|(name, value)| {
                value.map(|value| (name.to_string_lossy(), value.to_string_lossy()))
            })
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            values
                .get("OPENROUTER_BASE_URL")
                .map(|value| value.as_ref()),
            Some("http://127.0.0.1:3456/v1")
        );
        assert_eq!(
            values.get("ASB_TAPE_MODE").map(|value| value.as_ref()),
            Some("replay")
        );
    }

    #[test]
    fn project_tool_is_preferred_without_path_fallback() {
        let path = Path::new(".asb-tools/llmtape/node_modules/.bin/llmtape");
        assert_eq!(
            resolve_executable(Some(path)).unwrap(),
            path.display().to_string()
        );
    }

    #[test]
    fn missing_project_tool_explains_local_install() {
        let error = resolve_executable_with_override(None, None)
            .expect_err("missing llmtape should fail closed");
        assert!(error.to_string().contains("asb tool install llmtape"));
        assert!(error.to_string().contains("benchmark project"));
    }
}
