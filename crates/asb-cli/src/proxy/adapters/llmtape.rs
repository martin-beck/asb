//! Local llmtape proxy lifecycle for deterministic LLM recording and replay.

pub(crate) mod replay;
mod tool_calls;
pub(crate) mod usage;

use crate::Error;
use crate::proxy::{ProxyAdapter, ProxySession};
use crate::tape::Mode;
use std::any::Any;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const BUILTIN_PROXY: &str = include_str!("../../../assets/scripts/llmtape.mjs");
const NETWORK_FETCH: &str = include_str!("../../../assets/scripts/network-fetch.mjs");
const CAPTURE_SESSION: &str = include_str!("../../../assets/scripts/capture-session.mjs");
const TAPE_NORMALIZATION: &str = include_str!("../../../assets/scripts/tape-normalization.mjs");
static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);
static START_LOCK: Mutex<()> = Mutex::new(());

const NAME: &str = "llmtape";

struct Settings {
    mode: Mode,
    cassette: PathBuf,
    respect_record_timings: bool,
    workspace: PathBuf,
    /// Opaque adapter-owned request extension; llmtape materializes and
    /// invokes it without interpreting its wire protocol.
    wire_extension: Option<&'static str>,
}

struct RunningLlmtape {
    child: Option<Child>,
    builtin: bool,
    url: String,
    progress: Option<PathBuf>,
    mismatch_evidence: Option<PathBuf>,
    cassette: PathBuf,
    mode: Mode,
    cleanup_confirmed: bool,
    capture_request: Option<crate::proxy::capture::Request>,
    capture: Option<crate::proxy::capture::CaptureEvidence>,
    _scratch: Scratch,
}

pub(super) struct LlmtapeAdapter;
pub(super) static ADAPTER: LlmtapeAdapter = LlmtapeAdapter;

// A child created during startup must not survive any error path.
struct StartingChild(Option<Child>);

impl Drop for StartingChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl ProxyAdapter for LlmtapeAdapter {
    fn name(&self) -> &'static str {
        NAME
    }

    fn validate_replay_model(&self, path: &Path, model: &str) -> Result<(), Error> {
        replay::ensure_replay_model_recorded(path, model)
    }

    fn replay_interface(&self, path: &Path) -> Result<Option<crate::endpoint::Interface>, Error> {
        replay::replay_interface(path)
    }

    fn recorded_usage(&self, text: &str, input_includes_cache: bool) -> crate::usage::TokenUsage {
        usage::from_cassette(text, input_includes_cache)
    }

    fn recorded_tool_calls(&self, path: &Path) -> Option<Vec<crate::proxy::tool_calls::ToolCall>> {
        tool_calls::from_cassette(path)
    }

    fn default_recording(&self) -> bool {
        #[cfg(test)]
        if std::env::var_os("ASB_SYNTHETIC_RECORDER_CHILD").is_some() {
            return false;
        }
        true
    }

    fn supports_offline_replay(&self) -> bool {
        true
    }

    fn recording_settings(
        &self,
        request: &crate::proxy::RecordingRequest<'_>,
    ) -> Result<Box<dyn Any>, Error> {
        Ok(Box::new(Settings {
            mode: request.mode,
            cassette: request.cassette.to_path_buf(),
            respect_record_timings: request.respect_record_timings,
            workspace: request.workspace.to_path_buf(),
            wire_extension: request.wire_extension,
        }))
    }

    fn input_files(&self) -> Result<Vec<PathBuf>, Error> {
        execution_input_files(configured_override().as_deref())
    }
    fn runtime_inputs(&self) -> Result<Vec<crate::runtime_inputs::Dependency>, Error> {
        // Retain the logical command, not just its initial PATH resolution,
        // so a new earlier PATH candidate is detectable at final discovery.
        Ok(vec![crate::runtime_inputs::Dependency::program(
            configured_override().unwrap_or_else(|| "node".into()),
        )])
    }

    fn start(
        &self,
        upstream: Option<&str>,
        settings: Option<&dyn Any>,
    ) -> Result<Box<dyn ProxySession>, Error> {
        let request = settings
            .and_then(|settings| settings.downcast_ref::<Settings>())
            .ok_or_else(|| Error::Input("llmtape requires record/replay settings".into()))?;
        let recording = (request.mode == Mode::Record)
            .then(|| recording_destination(&request.cassette))
            .transpose()?;
        let cassette = recording.as_ref().unwrap_or(&request.cassette);
        let parent = cassette
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        // The embedded proxy accepts private receipt and mismatch paths only
        // when they are absolute. Relative cassette paths are valid CLI input,
        // so resolve their existing parent before allocating the owned scratch
        // directory used for those capabilities.
        let parent = std::fs::canonicalize(parent).map_err(|error| {
            Error::Run(format!(
                "cannot resolve tape session directory {}: {error}",
                parent.display()
            ))
        })?;
        let scratch = Scratch::create(&parent)?;
        let wire_extension = request
            .wire_extension
            .map(|source| {
                let path = scratch.path.join("agent-wire-extension.mjs");
                std::fs::write(&path, source).map_err(|error| {
                    Error::Run(format!(
                        "cannot materialize adapter tape extension: {error}"
                    ))
                })?;
                Ok::<_, Error>(path)
            })
            .transpose()?;
        // Every replay has its own copy and progress file. Sharing the source
        // directly would race on the proxy's cassette progress sidecar.
        let replay_cassette = scratch.path.join("cassette.jsonl");
        if request.mode == Mode::Replay {
            std::fs::copy(cassette, &replay_cassette).map_err(|error| {
                Error::Run(format!(
                    "cannot copy replay cassette {}: {error}",
                    cassette.display()
                ))
            })?;
        }
        // Legacy overrides still choose a released port: serialize sibling
        // startup, without claiming this excludes other ASB processes.
        let _start_guard = START_LOCK
            .lock()
            .map_err(|_| Error::Run("tape startup lock was poisoned".into()))?;
        let executable = resolve_executable(&scratch.path)?;
        // The built-in child binds its own ephemeral port and publishes it
        // only after listening. A released parent-selected port can be taken
        // by a different ASB process; a TCP connect does not prove ownership.
        let builtin = matches!(executable, ProxyExecutable::Builtin(_));
        let port = if builtin { 0 } else { free_port()? };
        let ready_file = scratch.path.join("ready.json");
        let mut command = executable.command();
        command.env_remove("ASB_TAPE_READY_FILE");
        command.env_remove("ASB_TAPE_OWNER_PIPE");
        command.env_remove("ASB_TAPE_CAPTURE_RECEIPT");
        command.env_remove("ASB_TAPE_CAPTURE_NONCE");
        command.env_remove("ASB_TAPE_MISMATCH_EVIDENCE");
        command.env_remove("ASB_TAPE_AGENT_WIRE_EXTENSION");
        if let Some(path) = wire_extension {
            command.env("ASB_TAPE_AGENT_WIRE_EXTENSION", path);
        }
        let capture = (builtin && request.mode == Mode::Record).then(|| {
            (
                scratch.path.join("capture.json"),
                crate::tools::runtime::unique_id(),
            )
        });
        if let Some((receipt, nonce)) = &capture {
            command.env("ASB_TAPE_CAPTURE_RECEIPT", receipt);
            command.env("ASB_TAPE_CAPTURE_NONCE", nonce);
        }
        if builtin {
            command.env("ASB_TAPE_READY_FILE", &ready_file);
            // The parent retains the write end in Child.stdin. CLOEXEC keeps
            // other launched programs from extending this lifetime lease.
            command.env("ASB_TAPE_OWNER_PIPE", "1");
        }
        crate::project::apply_network_proxy(&mut command, Path::new("."))?;
        command
            .arg(match request.mode {
                Mode::Record => "record",
                Mode::Replay => "replay",
            })
            .arg("--port")
            .arg(port.to_string());
        match request.mode {
            Mode::Record => {
                let upstream = upstream.ok_or_else(|| {
                    Error::Run(
                        "record requires a provider API endpoint; set the provider base URL or use its configured local bridge".into(),
                    )
                })?;
                command.args(["--upstream", upstream, "--out"]);
                command.arg(cassette);
            }
            Mode::Replay => {
                command.args(["--cassette"]);
                command.arg(&replay_cassette);
                command.env(
                    "ASB_TAPE_MISMATCH_EVIDENCE",
                    scratch.path.join("mismatch.json"),
                );
                if request.respect_record_timings {
                    command.arg("--respect-record-timings");
                }
            }
        }
        // Agents may serialize either the configured relative workspace or
        // the absolute path used by their process runner.  Give the tape
        // normalizer the canonical form so record/replay keys remain stable
        // when their result roots differ.
        let tape_workspace =
            std::fs::canonicalize(&request.workspace).unwrap_or_else(|_| request.workspace.clone());
        command
            .env("ASB_TAPE_WORKSPACE", tape_workspace)
            .stdin(if builtin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().map_err(|error| {
            let help = match (&executable, error.kind()) {
                (ProxyExecutable::Override(path), std::io::ErrorKind::PermissionDenied) => format!(
                    "Check parent-directory access; if you own the override executable, run `chmod u+x -- {}` and retry.", shell_quote(Path::new(path))
                ),
                (ProxyExecutable::Override(path), _) => format!("Check the ASB_LLM_TAPE executable at {} and retry.", path),
                (ProxyExecutable::Builtin(_), _) => "Install Node.js or set ASB_LLM_TAPE to a compatible executable; check Node with `node --version`.".to_owned(),
            };
            Error::Run(format!("cannot start the tape proxy: {error}. {help}"))
        })?;
        let mut child = StartingChild(Some(child));
        let port = wait_for_ready(
            &mut child,
            builtin.then_some(ready_file.as_path()),
            port,
            STARTUP_TIMEOUT,
        )?;
        let capture_request = capture.map(|(receipt, nonce)| crate::proxy::capture::Request {
            receipt,
            output: cassette.clone(),
            nonce,
            pid: child.0.as_ref().expect("ready child").id(),
        });
        Ok(Box::new(RunningLlmtape {
            child: child.0.take(),
            builtin,
            url: format!("http://127.0.0.1:{port}/v1"),
            progress: (request.mode == Mode::Replay)
                .then(|| replay_cassette.with_extension("jsonl.progress.json")),
            mismatch_evidence: (request.mode == Mode::Replay)
                .then(|| scratch.path.join("mismatch.json")),
            cassette: cassette.clone(),
            mode: request.mode,
            cleanup_confirmed: false,
            capture_request,
            capture: None,
            _scratch: scratch,
        }))
    }
}

/// Resolve the existing parent using filesystem semantics (including symlink/..)
/// once. The producer and receipt verifier receive this identical destination.
fn recording_destination(path: &Path) -> Result<PathBuf, Error> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| Error::Input("recording destination requires a filename".into()))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        Error::Run(format!(
            "cannot create cassette directory {}: {error}",
            parent.display()
        ))
    })?;
    let destination = parent.canonicalize().map_err(crate::io_error)?.join(name);
    match std::fs::symlink_metadata(&destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(destination),
        Err(error) => Err(crate::io_error(error)),
        Ok(_) => Err(Error::Input(format!(
            "recording destination {} already exists; choose a new cassette instead of overwriting retained evidence",
            destination.display()
        ))),
    }
}

fn wait_for_ready(
    child: &mut StartingChild,
    ready_file: Option<&Path>,
    legacy_port: u16,
    timeout: Duration,
) -> Result<u16, Error> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let process = child.0.as_mut().expect("startup child is present");
        if let Some(status) = process
            .try_wait()
            .map_err(|error| Error::Run(format!("cannot inspect tape proxy: {error}")))?
        {
            return Err(Error::Run(format!(
                "ASB's tape proxy exited before becoming ready with {status}; verify Node.js and the cassette format"
            )));
        }
        let port = if let Some(path) = ready_file {
            read_ready_port(path, process.id())?
        } else {
            // Explicit legacy overrides retain their fixed-port protocol.
            // Their listener ownership is the override's responsibility.
            TcpStream::connect_timeout(
                &SocketAddr::from((Ipv4Addr::LOCALHOST, legacy_port)),
                Duration::from_millis(100),
            )
            .is_ok()
            .then_some(legacy_port)
        };
        if let Some(port) = port {
            if process
                .try_wait()
                .map_err(|error| Error::Run(format!("cannot inspect tape proxy: {error}")))?
                .is_some()
            {
                return Err(Error::Run(
                    "tape proxy exited during readiness publication; no agent was started".into(),
                ));
            }
            return Ok(port);
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(Error::Run("ASB's tape proxy did not become ready within the startup deadline; verify Node.js and the cassette format".into()))
}

/// Accept only the private, bounded publication from the intended child.
fn read_ready_port(path: &Path, child_id: u32) -> Result<Option<u16>, Error> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::Run(format!("cannot read tape readiness: {error}"))),
    };
    if !file.metadata().map_err(crate::io_error)?.is_file() {
        return Err(Error::Run("tape readiness is not a regular file".into()));
    }
    let mut bytes = Vec::new();
    file.take(1025)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::Run(format!("cannot read tape readiness: {error}")))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Run("tape proxy published malformed readiness".into()))?;
    let port = value["port"]
        .as_u64()
        .and_then(|port| u16::try_from(port).ok());
    if bytes.len() > 1024
        || value["schema_version"] != 1
        || value["pid"] != child_id
        || value["host"] != "127.0.0.1"
        || port.is_none_or(|port| port == 0)
    {
        return Err(Error::Run(
            "tape proxy published invalid endpoint ownership; no agent was started".into(),
        ));
    }
    Ok(port)
}

impl ProxySession for RunningLlmtape {
    fn output_files(&self) -> Vec<PathBuf> {
        if self.mode == Mode::Record {
            vec![self.cassette.clone()]
        } else {
            Vec::new()
        }
    }
    fn cleanup_confirmed(&self) -> bool {
        self.cleanup_confirmed
    }
    fn capture_evidence(&self) -> Option<crate::proxy::capture::CaptureEvidence> {
        self.capture.clone()
    }
    fn url(&self) -> &str {
        &self.url
    }

    fn finish(&mut self) -> Result<(), Error> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let stopped = stop_proxy(&mut child, self.builtin);
        self.cleanup_confirmed = stopped.is_ok();
        // A valid receipt survives a later cleanup failure. Verify it even when
        // exit status cannot establish resource cleanup, and keep both facts.
        let captured = self
            .capture_request
            .take()
            .map(|request| request.read(NAME));
        let capture_error = match captured {
            Some(Ok(evidence)) => {
                // Incomplete captures have no completion receipt. Keep their
                // bounded private delivery diagnostic beside the recorder
                // receipt for inspection rather than deleting it on drop.
                self._scratch.retain |= !evidence.complete;
                self.capture = Some(evidence);
                None
            }
            Some(Err(error)) => Some(error),
            None => None,
        };
        if stopped.is_err() || capture_error.is_some() {
            self._scratch.retain = true;
        }
        stopped.map_err(|error| {
            Error::Run(format!(
                "could not stop proxy adapter: {error}; retained recorder state at {}",
                self._scratch.path.display()
            ))
        })?;
        if let Some(error) = capture_error {
            return Err(Error::Run(format!(
                "{error}; retained recorder state at {}",
                self._scratch.path.display()
            )));
        }
        if self.mode == Mode::Replay {
            let progress = self
                .progress
                .as_ref()
                .ok_or_else(|| Error::Run("replay cassette progress path is missing".into()))?;
            let text = std::fs::read_to_string(progress).map_err(|error| Error::Run(format!(
                "replay did not produce cassette progress for {}; verify the tape proxy: {error}", self.cassette.display()
            )))?;
            let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
                Error::Run(format!("replay cassette progress is malformed: {error}"))
            })?;
            let consumed = value
                .get("consumed")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let total = value
                .get("total")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            let rejected = value
                .get("rejected")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            if rejected != 0 {
                let mismatch = replay_mismatch_context(&value)
                    .map(|context| format!("; first mismatch: {context}"))
                    .unwrap_or_default();
                let evidence = self
                    .mismatch_evidence
                    .as_deref()
                    .filter(|path| path.is_file())
                    .and_then(|path| self._scratch.retain_mismatch_evidence(path).ok())
                    .map(|path| {
                        format!("; private mismatch evidence retained at {}", path.display())
                    })
                    .unwrap_or_default();
                return Err(Error::Run(format!(
                    "replay rejected {rejected} request(s) for {}{mismatch}{evidence}; the normalized request or sequence differs from the recording",
                    self.cassette.display(),
                )));
            }
            if consumed != total {
                return Err(Error::Run(format!(
                    "replay consumed {consumed} of {total} cassette requests for {}; refusing partial replay",
                    self.cassette.display()
                )));
            }
        }
        Ok(())
    }

    fn abort(&mut self) {
        if let Some(mut child) = self.child.take() {
            self.cleanup_confirmed = stop_proxy(&mut child, self.builtin).is_ok();
            self._scratch.retain |= !self.cleanup_confirmed;
        }
    }
}

fn replay_mismatch_context(progress: &serde_json::Value) -> Option<String> {
    let mismatch = progress.get("first_mismatch")?.as_object()?;
    let sequence = mismatch.get("sequence")?.as_u64()?;
    let category = mismatch.get("category")?.as_str()?;
    let path = mismatch.get("path")?.as_str()?;
    let safe_category = matches!(
        category,
        "request"
            | "method"
            | "url"
            | "body"
            | "json_type"
            | "array_length"
            | "object_keys"
            | "json_value"
            | "body_size_limit"
            | "cassette_exhausted"
    );
    let safe_path = !path.is_empty()
        && path.len() <= 256
        && path.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'$' | b'.' | b'[' | b']' | b'<' | b'>' | b'_' | b'-')
        });
    if !(safe_category && safe_path) {
        return None;
    }
    let string_detail = mismatch
        .get("string_lengths")
        .and_then(serde_json::Value::as_object)
        .and_then(|lengths| {
            Some((
                lengths.get("expected")?.as_u64()?,
                lengths.get("actual")?.as_u64()?,
                mismatch.get("first_difference_offset")?.as_u64()?,
            ))
        })
        .filter(|(expected, actual, offset)| {
            *expected <= 16 * 1024 * 1024
                && *actual <= 16 * 1024 * 1024
                && *offset <= (*expected).min(*actual)
        })
        .map(|(expected, actual, offset)| {
            format!("; string lengths {expected}→{actual}, first differing byte {offset}")
        })
        .unwrap_or_default();
    let feature_detail = replay_string_feature_context(mismatch).unwrap_or_default();
    Some(format!(
        "request {sequence}, {category} at {path}{string_detail}{feature_detail}"
    ))
}

fn replay_string_feature_context(
    mismatch: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let features = mismatch.get("string_features")?.as_object()?;
    let expected = features.get("expected")?.as_object()?;
    let actual = features.get("actual")?.as_object()?;
    let lines = |value: &serde_json::Map<String, serde_json::Value>| {
        value
            .get("lines")?
            .as_u64()
            .filter(|lines| *lines <= 65_536)
    };
    let flags = [
        ("has_agents_guidance", "AGENTS guidance"),
        ("has_parent_path", "parent paths"),
        ("has_workspace_path", "workspace paths"),
        ("has_additional_input", "additional-input marker"),
    ];
    let changed = flags
        .iter()
        .filter_map(|(key, label)| {
            let expected = expected.get(*key)?.as_bool()?;
            let actual = actual.get(*key)?.as_bool()?;
            (expected != actual && actual).then_some(*label)
        })
        .collect::<Vec<_>>();
    let line_detail = match (lines(expected), lines(actual)) {
        (Some(expected), Some(actual)) if expected != actual => {
            format!("; lines {expected}→{actual}")
        }
        _ => String::new(),
    };
    (!line_detail.is_empty() || !changed.is_empty()).then(|| {
        format!(
            "{line_detail}{}",
            if changed.is_empty() {
                String::new()
            } else {
                format!("; replay adds {}", changed.join(", "))
            }
        )
    })
}

fn stop_proxy(child: &mut Child, builtin: bool) -> std::io::Result<()> {
    let was_running = child.try_wait()?.is_none();
    crate::project::stop_network_child(child)?;
    let status = child.try_wait()?;
    // A running built-in producer is deliberately terminated by ASB. Its
    // signal status is not a failed cleanup when the owned child was reaped.
    // An already-exited producer must still have succeeded on its own.
    if builtin && !was_running && !status.is_some_and(|status| status.success()) {
        return Err(std::io::Error::other(
            "embedded tape proxy exited without confirming owned subprocess cleanup; inspect retained attempt artifacts",
        ));
    }
    Ok(())
}

impl Drop for RunningLlmtape {
    fn drop(&mut self) {
        self.abort();
    }
}

/// A tape proxy command. The default proxy is compiled into ASB and materialized
/// beside the cassette; no npm package or catalog installation is involved.
enum ProxyExecutable {
    Builtin(PathBuf),
    Override(String),
}

impl ProxyExecutable {
    fn command(&self) -> Command {
        match self {
            Self::Builtin(script) => {
                let mut command = Command::new("node");
                command.arg(script);
                command
            }
            Self::Override(executable) => Command::new(executable),
        }
    }
}

/// Resolve an explicitly supplied proxy or materialize ASB's built-in proxy.
/// ASB deliberately does not look up an unqualified host `llmtape` command.
fn resolve_executable(directory: &Path) -> Result<ProxyExecutable, Error> {
    if let Some(override_path) = configured_override() {
        return Ok(ProxyExecutable::Override(override_path));
    }
    let script = directory.join(".asb-tape-proxy.mjs");
    std::fs::write(directory.join("network-fetch.mjs"), NETWORK_FETCH)
        .map_err(|error| Error::Run(format!("cannot materialize tape network client: {error}")))?;
    std::fs::write(directory.join("capture-session.mjs"), CAPTURE_SESSION).map_err(|error| {
        Error::Run(format!(
            "cannot materialize tape capture component: {error}"
        ))
    })?;
    std::fs::write(directory.join("tape-normalization.mjs"), TAPE_NORMALIZATION)
        .map_err(|error| Error::Run(format!("cannot materialize tape normalization: {error}")))?;
    std::fs::write(&script, BUILTIN_PROXY).map_err(|error| {
        Error::Run(format!(
            "cannot materialize ASB's built-in tape proxy at {}: {error}",
            script.display()
        ))
    })?;
    Ok(ProxyExecutable::Builtin(script))
}

fn configured_override() -> Option<String> {
    std::env::var("ASB_LLM_TAPE")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn execution_input_files(override_path: Option<&str>) -> Result<Vec<PathBuf>, Error> {
    // The built-in scripts are already bound by ASB's executable identity.
    // Keep the actual command's host runtime or explicit override separately.
    let program = Path::new(override_path.unwrap_or("node"));
    let path = crate::tools::host_executable(program).ok_or_else(|| {
        Error::Input("selected tape proxy executable is unavailable for input binding".into())
    })?;
    Ok(vec![path])
}

// Created exclusively, so teardown only removes files owned by this session.
struct Scratch {
    path: PathBuf,
    retain: bool,
}

impl Scratch {
    fn create(parent: &Path) -> Result<Self, Error> {
        for _ in 0..100 {
            let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".asb-tape-{}-{id}", std::process::id()));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        retain: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    let help = if error.kind() == std::io::ErrorKind::PermissionDenied {
                        format!(
                            " Check parent-directory access; if you own the directory, run `chmod u+rwx -- {}` and retry.",
                            shell_quote(parent)
                        )
                    } else {
                        String::new()
                    };
                    return Err(Error::Run(format!(
                        "cannot create a tape session directory in {}: {error}.{help}",
                        parent.display()
                    )));
                }
            }
        }
        Err(Error::Run(
            "cannot allocate a unique tape session directory".into(),
        ))
    }

    fn retain_mismatch_evidence(&mut self, evidence: &Path) -> Result<PathBuf, Error> {
        let name = self.path.file_name().ok_or_else(|| {
            Error::Run("cannot derive private replay mismatch evidence name".into())
        })?;
        let destination = self
            .path
            .with_file_name(format!("{}.mismatch.json", name.to_string_lossy()));
        std::fs::rename(evidence, &destination).map_err(|error| {
            Error::Run(format!(
                "cannot retain private replay mismatch evidence at {}: {error}",
                destination.display()
            ))
        })?;
        Ok(destination)
    }
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.retain {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn free_port() -> Result<u16, Error> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map(|listener| listener.local_addr().map(|address| address.port()))
        .map_err(|error| Error::Run(format!("cannot reserve a local llmtape port: {error}")))?
        .map_err(|error| Error::Run(format!("cannot inspect local llmtape port: {error}")))
}

#[cfg(all(test, unix))]
#[path = "../../../tests/unit/llmtape_boundaries.rs"]
mod boundary_tests;

#[cfg(test)]
mod tests {
    use super::{
        ProxyExecutable, StartingChild, read_ready_port, resolve_executable, wait_for_ready,
    };
    use crate::proxy::parse_chain;
    use crate::tape::{Mode, Session};
    use std::io::{Read, Write};
    use std::net::TcpStream;

    #[test]
    fn abnormal_builtin_exit_cannot_confirm_proxy_cleanup() {
        use crate::proxy::ProxySession;
        let root = test_root();
        for code in [0, 2] {
            let mut child = std::process::Command::new("node")
                .args(["-e", &format!("process.exit({code})")])
                .spawn()
                .unwrap();
            child.wait().unwrap();
            let mut session = super::RunningLlmtape {
                child: Some(child),
                builtin: true,
                url: String::new(),
                progress: None,
                mismatch_evidence: None,
                cassette: root.join("tape"),
                mode: Mode::Record,
                cleanup_confirmed: false,
                capture_request: None,
                capture: None,
                _scratch: super::Scratch::create(&root).unwrap(),
            };
            assert_eq!(session.finish().is_ok(), code == 0);
            assert_eq!(session.cleanup_confirmed(), code == 0);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn builtin_startup_rejects_foreign_listener_and_reaps_unready_child() {
        let root = test_root();
        let foreign = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = foreign.local_addr().unwrap();
        assert!(TcpStream::connect(address).is_ok());
        let process = std::process::Command::new("node")
            .args(["-e", "setInterval(()=>{},1000)"])
            .spawn()
            .unwrap();
        let pid = process.id();
        let mut child = StartingChild(Some(process));
        let result = wait_for_ready(
            &mut child,
            Some(&root.join("absent.json")),
            address.port(),
            std::time::Duration::from_millis(150),
        );
        assert!(
            result.is_err(),
            "foreign listener must never supply readiness"
        );
        assert!(child.0.as_mut().unwrap().try_wait().unwrap().is_none());
        drop(child);
        #[cfg(target_os = "linux")]
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        #[cfg(not(target_os = "linux"))]
        let _ = pid;
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn builtin_publishes_private_receipt_for_its_served_port() {
        let root = test_root();
        let cassette = cassette(&root);
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let ready = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("ready.json"))
            .find(|path| path.is_file())
            .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&ready).unwrap()).unwrap();
        assert_eq!(
            session.url(),
            format!("http://127.0.0.1:{}/v1", value["port"])
        );
        assert!(value["pid"].as_u64().unwrap() > 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&ready).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(request(&session, r#"{"model":"m","messages":[{"content":"work in <asb-workspace>"}],"tools":[{"type":"function"}]}"#).contains("200 OK"));
        session.stop().unwrap();
        assert!(!ready.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unrelated_listener_cannot_supply_builtin_readiness() {
        let root = test_root();
        let ready = root.join("ready.json");
        let unrelated = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = unrelated.local_addr().unwrap();
        assert!(TcpStream::connect(address).is_ok());
        assert_eq!(read_ready_port(&ready, 42).unwrap(), None);
        for invalid in [
            serde_json::json!({"schema_version":1,"pid":43,"host":"127.0.0.1","port":address.port()}),
            serde_json::json!({"schema_version":1,"pid":42,"host":"0.0.0.0","port":address.port()}),
            serde_json::json!({"schema_version":1,"pid":42,"host":"127.0.0.1","port":0}),
            serde_json::json!({"schema_version":1,"pid":42,"host":"127.0.0.1","port":65536}),
            serde_json::json!({"schema_version":2,"pid":42,"host":"127.0.0.1","port":address.port()}),
        ] {
            std::fs::write(&ready, invalid.to_string()).unwrap();
            assert!(read_ready_port(&ready, 42).is_err());
        }
        let valid = serde_json::json!({"schema_version":1,"pid":42,"host":"127.0.0.1","port":address.port()}).to_string();
        std::fs::write(&ready, &valid).unwrap();
        assert_eq!(read_ready_port(&ready, 42).unwrap(), Some(address.port()));
        std::fs::write(&ready, format!("{valid}{}", " ".repeat(1025))).unwrap();
        assert!(read_ready_port(&ready, 42).is_err());
        std::fs::write(&ready, b"not json").unwrap();
        assert!(read_ready_port(&ready, 42).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    #[test]
    fn declared_override_input_is_not_launched_or_materialized_and_keeps_its_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = test_root();
        let program = root.join("proxy");
        let marker = root.join("must-not-run");
        std::fs::write(&program, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let link = root.join("selected-proxy");
        symlink(&program, &link).unwrap();
        let files = super::execution_input_files(link.to_str()).unwrap();
        assert_eq!(files.as_slice(), std::slice::from_ref(&link));
        assert!(!marker.exists());
        assert!(!root.join(".asb-tape-proxy.mjs").exists());
        std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(super::execution_input_files(link.to_str()).is_err());
        assert!(!marker.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn test_root() -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("asb-tape-session-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn cassette(root: &Path) -> std::path::PathBuf {
        let path = root.join("record.jsonl");
        let body = r#"{"model":"m","messages":[{"content":"work in <asb-workspace>"}],"tools":[{"type":"function"}]}"#;
        let line = serde_json::json!({"request":{"method":"POST","url":"/v1/chat/completions","body":body},"response":{"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}});
        std::fs::write(&path, format!("{line}\n")).unwrap();
        path
    }

    fn request(session: &Session, body: &str) -> String {
        request_to(session, "/v1/chat/completions", body)
    }

    fn request_to(session: &Session, path: &str, body: &str) -> String {
        let address = session
            .url()
            .trim_start_matches("http://")
            .trim_end_matches("/v1");
        let mut socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        write!(socket, "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn eight_sessions_replay_one_cassette_independently() {
        let root = test_root();
        let cassette = cassette(&root);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8).map(|index| {
                let cassette = &cassette;
                scope.spawn(move || {
                    let workspace = format!("/tmp/replica-{index}");
                    let session = Session::start(Mode::Replay, cassette, None, false, Path::new(&workspace)).unwrap();
                    let body = format!(r#"{{"tools":[{{"type":"function"}}],"messages":[{{"content":"work in {workspace}"}}],"model":"m"}}"#);
                    assert!(request(&session, &body).contains("200 OK"));
                    session.stop().unwrap();
                })
            }).collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            1,
            "session artifacts cleaned up"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_waits_for_an_earlier_concurrent_request() {
        let root = test_root();
        let cassette = root.join("ordered.jsonl");
        let first = r#"{"model":"m","messages":[{"content":"first"}]}"#;
        let second = r#"{"model":"m","messages":[{"content":"second"}]}"#;
        let line = |body: &str| {
            serde_json::json!({"request":{"method":"POST","url":"/v1/chat/completions","body":body},"response":{"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"},"delay_ms":if body == second { 150 } else { 0 }}).to_string()
        };
        std::fs::write(&cassette, format!("{}\n{}\n", line(first), line(second))).unwrap();
        let session = Session::start(
            Mode::Replay,
            &cassette,
            None,
            true,
            Path::new("/tmp/workspace"),
        )
        .unwrap();
        let address = session
            .url()
            .trim_start_matches("http://")
            .trim_end_matches("/v1");
        let mut later = TcpStream::connect(address).unwrap();
        later
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        write!(later, "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{second}", second.len()).unwrap();
        let mut probe = [0u8; 1];
        let early = later.read(&mut probe);
        let started = std::time::Instant::now();
        let earlier_response = request(&session, first);
        later
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut later_response = Vec::new();
        if let Ok(n) = early {
            later_response.extend_from_slice(&probe[..n]);
        }
        let _ = later.read_to_end(&mut later_response);
        let elapsed = started.elapsed();
        let stopped = session.stop();
        assert!(
            early.is_err(),
            "later request received an immediate response"
        );
        assert!(earlier_response.contains("200 OK"), "{earlier_response}");
        assert!(String::from_utf8_lossy(&later_response).contains("200 OK"));
        assert!(
            elapsed >= std::time::Duration::from_millis(100),
            "recorded response delay was not respected: {elapsed:?}"
        );
        assert!(stopped.is_ok(), "{stopped:?}");

        // A missing earlier request must not make proxy shutdown wait for the
        // ordering timeout or claim the cassette was completely consumed.
        let session = Session::start(
            Mode::Replay,
            &cassette,
            None,
            false,
            Path::new("/tmp/workspace"),
        )
        .unwrap();
        let address = session
            .url()
            .trim_start_matches("http://")
            .trim_end_matches("/v1");
        let mut waiting = TcpStream::connect(address).unwrap();
        waiting
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        write!(waiting, "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{second}", second.len()).unwrap();
        assert!(waiting.read(&mut [0u8; 1]).is_err());
        let started = std::time::Instant::now();
        assert!(session.stop().is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replay_waits_after_skipping_a_recorded_probe() {
        let root = test_root();
        let cassette = root.join("probe-ordered.jsonl");
        let first = r#"{"model":"m","messages":[{"content":"first"}]}"#;
        let second = r#"{"model":"m","messages":[{"content":"second"}]}"#;
        let probe = serde_json::json!({"request":{"method":"GET","url":"/v1/models","body":null},"response":{"status":200,"headers":{},"body":"{}"}});
        let line = |body: &str| serde_json::json!({"request":{"method":"POST","url":"/v1/chat/completions","body":body},"response":{"status":200,"headers":{},"body":"{}"}});
        std::fs::write(
            &cassette,
            format!("{probe}\n{}\n{}\n", line(first), line(second)),
        )
        .unwrap();
        let session = Session::start(
            Mode::Replay,
            &cassette,
            None,
            false,
            Path::new("/tmp/workspace"),
        )
        .unwrap();
        let address = session
            .url()
            .trim_start_matches("http://")
            .trim_end_matches("/v1");
        let mut later = TcpStream::connect(address).unwrap();
        later
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        write!(later, "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{second}", second.len()).unwrap();
        let mut byte = [0u8; 1];
        let early = later.read(&mut byte);
        let first_response = request(&session, first);
        later
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut response = Vec::new();
        if let Ok(n) = early {
            response.extend_from_slice(&byte[..n]);
        }
        let _ = later.read_to_end(&mut response);
        let stopped = session.stop();
        let _ = std::fs::remove_dir_all(root);
        assert!(
            early.is_err(),
            "later request was rejected after probe skipping"
        );
        assert!(first_response.contains("200 OK"), "{first_response}");
        assert!(String::from_utf8_lossy(&response).contains("200 OK"));
        assert!(stopped.is_ok(), "{stopped:?}");
    }

    #[test]
    fn queued_requests_wait_for_a_delayed_recorded_probe() {
        let root = test_root();
        let cassette = root.join("delayed-probe.jsonl");
        let first = r#"{"model":"m","messages":[{"content":"first"}]}"#;
        let later_body = r#"{"model":"m","messages":[{"content":"later"}]}"#;
        for later_is_post in [false, true] {
            let row = |method: &str, url: &str, body: Option<&str>| serde_json::json!({"request":{"method":method,"url":url,"body":body},"response":{"status":200,"headers":{},"body":"{}"}});
            let later_method = if later_is_post { "POST" } else { "GET" };
            let later_path = if later_is_post {
                "/v1/chat/completions"
            } else {
                "/v1/other"
            };
            let body = if later_is_post { later_body } else { "" };
            std::fs::write(
                &cassette,
                format!(
                    "{}\n{}\n{}\n",
                    row("POST", "/v1/chat/completions", Some(first)),
                    row("GET", "/v1/models", None),
                    row(later_method, later_path, (!body.is_empty()).then_some(body))
                ),
            )
            .unwrap();
            let session = Session::start(
                Mode::Replay,
                &cassette,
                None,
                false,
                Path::new("/tmp/workspace"),
            )
            .unwrap();
            let address = session
                .url()
                .trim_start_matches("http://")
                .trim_end_matches("/v1");
            let mut later = TcpStream::connect(address).unwrap();
            later
                .set_read_timeout(Some(std::time::Duration::from_millis(200)))
                .unwrap();
            write!(later, "{later_method} {later_path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            let mut byte = [0u8; 1];
            let early = later.read(&mut byte);
            let first_response = request(&session, first);
            let mut probe = TcpStream::connect(address).unwrap();
            probe
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            write!(
                probe,
                "GET /v1/models HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut probe_response = String::new();
            probe.read_to_string(&mut probe_response).unwrap();
            later
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut later_response = Vec::new();
            if let Ok(n) = early {
                later_response.extend_from_slice(&byte[..n]);
            }
            let _ = later.read_to_end(&mut later_response);
            let stopped = session.stop();
            assert!(early.is_err(), "later request returned before first");
            assert!(first_response.contains("200 OK"), "{first_response}");
            assert!(probe_response.contains("200 OK"), "{probe_response}");
            assert!(
                String::from_utf8_lossy(&later_response).contains("200 OK"),
                "{}",
                String::from_utf8_lossy(&later_response)
            );
            assert!(stopped.is_ok(), "{stopped:?}");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn omitted_queued_probe_is_skipped_without_waiting_for_deadline() {
        let root = test_root();
        let cassette = root.join("omitted-probe.jsonl");
        let first = r#"{"model":"m","messages":[{"content":"first"}]}"#;
        let later_body = r#"{"model":"m","messages":[{"content":"later"}]}"#;
        let row = |method: &str, url: &str, body: Option<&str>| serde_json::json!({"request":{"method":method,"url":url,"body":body},"response":{"status":200,"headers":{},"body":"{}"}});
        std::fs::write(
            &cassette,
            format!(
                "{}\n{}\n{}\n",
                row("POST", "/v1/chat/completions", Some(first)),
                row("GET", "/v1/models", None),
                row("POST", "/v1/chat/completions", Some(later_body))
            ),
        )
        .unwrap();
        let session = Session::start(
            Mode::Replay,
            &cassette,
            None,
            false,
            Path::new("/tmp/workspace"),
        )
        .unwrap();
        let address = session
            .url()
            .trim_start_matches("http://")
            .trim_end_matches("/v1");
        let mut later = TcpStream::connect(address).unwrap();
        later
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        write!(later, "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{later_body}", later_body.len()).unwrap();
        assert!(later.read(&mut [0u8; 1]).is_err());
        let started = std::time::Instant::now();
        let earlier = request(&session, first);
        later
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut response = String::new();
        later.read_to_string(&mut response).unwrap();
        let elapsed = started.elapsed();
        let stopped = session.stop();
        let _ = std::fs::remove_dir_all(root);
        assert!(earlier.contains("200 OK"));
        assert!(response.contains("200 OK"), "{response}");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "omitted probe delayed replay: {elapsed:?}"
        );
        assert!(stopped.is_ok(), "{stopped:?}");
    }

    #[test]
    fn omitted_probe_does_not_block_a_queued_bodyless_request() {
        let root = test_root();
        let cassette = root.join("omitted-probe-get.jsonl");
        let first = r#"{"model":"m","messages":[{"content":"first"}]}"#;
        let row = |method: &str, path: &str, body: Option<&str>| serde_json::json!({"request":{"method":method,"url":path,"body":body},"response":{"status":200,"headers":{},"body":"{}"}});
        std::fs::write(
            &cassette,
            format!(
                "{}\n{}\n{}\n",
                row("POST", "/v1/chat/completions", Some(first)),
                row("GET", "/v1/models", None),
                row("GET", "/v1/other", None)
            ),
        )
        .unwrap();
        let session = Session::start(
            Mode::Replay,
            &cassette,
            None,
            false,
            Path::new("/tmp/workspace"),
        )
        .unwrap();
        let address = session
            .url()
            .trim_start_matches("http://")
            .trim_end_matches("/v1");
        let mut later = TcpStream::connect(address).unwrap();
        later
            .set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .unwrap();
        write!(
            later,
            "GET /v1/other HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let early = later.read(&mut [0u8; 1]);
        let earlier = request(&session, first);
        later
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut response = String::new();
        let read = later.read_to_string(&mut response);
        let stopped = session.stop();
        let _ = std::fs::remove_dir_all(root);
        assert!(early.is_err());
        assert!(earlier.contains("200 OK"));
        assert!(read.is_ok(), "bodyless request did not proceed: {read:?}");
        assert!(response.contains("200 OK"), "{response}");
        assert!(stopped.is_ok(), "{stopped:?}");
    }

    #[test]
    fn a_different_bodyless_probe_does_not_skip_the_current_probe() {
        let root = test_root();
        let cassette = root.join("distinct-probe.jsonl");
        let row = serde_json::json!({"request":{"method":"GET","url":"/v1/models","body":null},"response":{"status":200,"headers":{},"body":"models"}});
        std::fs::write(&cassette, format!("{row}\n")).unwrap();
        let session = Session::start(
            Mode::Replay,
            &cassette,
            None,
            false,
            Path::new("/tmp/workspace"),
        )
        .unwrap();
        let get = |path: &str| {
            let address = session
                .url()
                .trim_start_matches("http://")
                .trim_end_matches("/v1");
            let mut socket = TcpStream::connect(address).unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            write!(
                socket,
                "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).unwrap();
            response
        };
        let wrong = get("/v1/health");
        let right = get("/v1/models");
        let stopped = session.stop();
        let _ = std::fs::remove_dir_all(root);
        assert!(wrong.contains("409 Conflict"), "{wrong}");
        assert!(right.contains("models"), "{right}");
        assert!(stopped.is_err(), "the rejected probe must remain visible");
    }

    #[test]
    fn skipped_duplicate_probes_are_available_once_each() {
        let root = test_root();
        let cassette = root.join("duplicate-probes.jsonl");
        let body = r#"{"model":"m","messages":[{"content":"first"}]}"#;
        let row = |method: &str, path: &str, request: Option<&str>, response: &str| serde_json::json!({"request":{"method":method,"url":path,"body":request},"response":{"status":200,"headers":{},"body":response},"delay_ms":if response == "probe-one" { 150 } else { 0 }});
        std::fs::write(
            &cassette,
            format!(
                "{}\n{}\n{}\n",
                row("GET", "/v1/models", None, "probe-one"),
                row("GET", "/v1/models", None, "probe-two"),
                row("POST", "/v1/chat/completions", Some(body), "workload")
            ),
        )
        .unwrap();
        let session = Session::start(
            Mode::Replay,
            &cassette,
            None,
            true,
            Path::new("/tmp/workspace"),
        )
        .unwrap();
        let workload = request(&session, body);
        let get = |path: &str| {
            let address = session
                .url()
                .trim_start_matches("http://")
                .trim_end_matches("/v1");
            let mut socket = TcpStream::connect(address).unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            write!(
                socket,
                "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).unwrap();
            response
        };
        let wrong = get("/v1/health");
        let started = std::time::Instant::now();
        let first = get("/v1/models");
        let elapsed = started.elapsed();
        let second = get("/v1/models");
        let extra = get("/v1/models");
        let stopped = session.stop();
        let _ = std::fs::remove_dir_all(root);
        assert!(workload.contains("200 OK"));
        assert!(wrong.contains("409 Conflict"), "{wrong}");
        assert!(first.contains("probe-one"), "{first}");
        assert!(
            elapsed >= std::time::Duration::from_millis(100),
            "deferred probe ignored recorded timing: {elapsed:?}"
        );
        assert!(second.contains("probe-two"), "{second}");
        assert!(extra.contains("409 Conflict"), "{extra}");
        assert!(stopped.is_err(), "mismatches must remain visible");
    }

    #[test]
    fn queued_duplicate_requests_consume_distinct_recorded_responses() {
        let root = test_root();
        let cassette = root.join("duplicate-ordered.jsonl");
        let first = r#"{"model":"m","messages":[{"content":"first"}]}"#;
        let repeated = r#"{"model":"m","messages":[{"content":"repeat"}]}"#;
        let line = |body: &str, response: &str| serde_json::json!({"request":{"method":"POST","url":"/v1/chat/completions","body":body},"response":{"status":200,"headers":{},"body":response}});
        std::fs::write(
            &cassette,
            format!(
                "{}\n{}\n{}\n",
                line(first, "first"),
                line(repeated, "reply-one"),
                line(repeated, "reply-two")
            ),
        )
        .unwrap();
        let session = Session::start(
            Mode::Replay,
            &cassette,
            None,
            false,
            Path::new("/tmp/workspace"),
        )
        .unwrap();
        let address = session
            .url()
            .trim_start_matches("http://")
            .trim_end_matches("/v1");
        let mut sockets = Vec::new();
        for _ in 0..2 {
            let mut socket = TcpStream::connect(address).unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_millis(100)))
                .unwrap();
            write!(socket, "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{repeated}", repeated.len()).unwrap();
            let mut byte = [0u8; 1];
            assert!(
                socket.read(&mut byte).is_err(),
                "duplicate arrived before its turn"
            );
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            sockets.push(socket);
        }
        let first_response = request(&session, first);
        let responses: Vec<String> = sockets
            .into_iter()
            .map(|mut socket| {
                let mut text = String::new();
                socket.read_to_string(&mut text).unwrap();
                text
            })
            .collect();
        let stopped = session.stop();
        let _ = std::fs::remove_dir_all(root);
        assert!(first_response.contains("200 OK"));
        assert_eq!(
            responses
                .iter()
                .filter(|response| response.contains("reply-one"))
                .count(),
            1,
            "{responses:?}"
        );
        assert_eq!(
            responses
                .iter()
                .filter(|response| response.contains("reply-two"))
                .count(),
            1,
            "{responses:?}"
        );
        assert!(stopped.is_ok(), "{stopped:?}");
    }

    #[test]
    fn replay_normalizes_sweep_result_workspace_slugs() {
        let root = test_root();
        let cassette = root.join("sweep-workspace.jsonl");
        let recorded = r#"{"model":"m","messages":[{"content":"write to srv-data-projects-asb-e2e-v0141-project-sweep-record-v0141-results-003-v0141-workbuddy-swe-bench-workspace/memory"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let workspace = Path::new(
            "/srv/data/projects/asb-e2e-v0142-project/sweep-replay-v0142-results/003-v0141-workbuddy-swe-bench/workspace",
        );
        let session = Session::start(Mode::Replay, &cassette, None, false, workspace).unwrap();
        let replayed = r#"{"model":"m","messages":[{"content":"write to srv-data-projects-asb-e2e-v0142-project-sweep-replay-v0142-results-003-v0141-workbuddy-swe-bench-workspace/memory"}]}"#;
        assert!(request(&session, replayed).contains("200 OK"));
        session.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_workbuddy_client_transport_envelope() {
        let root = test_root();
        let cassette = root.join("workbuddy-transport.jsonl");
        let recorded = r#"{"model":"m","cache_retention":"long","max_tokens":8192,"reasoning_effort":"medium","temperature":0,"verbosity":"low","extra_vars":{"initiator":"user"},"messages":[{"role":"system","content":"You are CodeBuddy Code.\n<codebuddy_background_info>\nYou are powered by the model named GPT-6.1-Sol. The exact model ID is gpt-6.1-sol."},{"role":"user","content":[{"text":"memory: srv-data-projects-live-results-asb-executions-record-1-batch-1-workspace/memory"}]},{"role":"assistant","messageId":"11111111111111111111111111111111","requestModelId":"gpt-6.1-sol","requestModelName":"GPT-6.1-Sol","content":"done"}],"tools":[{"type":"function","function":{"name":"read_file"}}],"stream":true}"#;
        let replayed = r#"{"model":"m","messages":[{"role":"system","content":"You are CodeBuddy Code.\n<codebuddy_background_info>\nYou are powered by the model named gpt-6.1-sol. The exact model ID is gpt-6.1-sol."},{"role":"user","content":[{"text":"memory: srv-data-projects-live-results-asb-executions-replay-2-batch-1-workspace/memory"}]},{"role":"assistant","messageId":"22222222222222222222222222222222","requestModelId":"custom-local:gpt-6.1-sol","requestModelName":"gpt-6.1-sol","content":"done"}],"tools":[{"type":"function","function":{"name":"read_file"}}],"stream":true}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200, "headers":{"content-type":"application/json"}, "body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, replayed).contains("200 OK"));
        session.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_opencode_tool_output_rendering_volatility() {
        let root = test_root();
        let cassette = root.join("opencode-tool-output.jsonl");
        let recorded = r#"{"model":"m","messages":[{"content":"Full output saved to: /opt/asb-private/agent-state/data/opencode/tool-output/tool_177aa249e001DOifK3c9XKyBvu\nfunction = <function concatenate at 0x72bd0ba330b0>\n14 failed, 30 passed, 7 skipped in 1.22s"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let replayed = r#"{"model":"m","messages":[{"content":"Full output saved to: /opt/asb-private/agent-state/data/opencode/tool-output/tool_178bfad560010MLrjXm7utidgk\nfunction = <function concatenate at 0x748aa3afbe30>\n14 failed, 30 passed, 7 skipped in 1.59s"}]}"#;
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let response = request(&session, replayed);
        assert!(response.contains("200 OK"), "{response}");
        session.stop().unwrap();

        let changed_result = replayed.replace("14 failed, 30 passed", "13 failed, 31 passed");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_result).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_qwen_persisted_output_filename() {
        let root = test_root();
        let cassette = root.join("qwen-persisted-output.jsonl");
        let recorded = r#"{"model":"m","messages":[{"content":"<persisted-output>\nOutput too large (37 KB). Full output saved to: /opt/asb-private/agent-state/home/.qwen/tmp/427298c01319355aae99da720b647df7cd291b9e3d2f3b5950ed35fb285aaf3f/tool-results/call_193d25ef253b4045a147311558349385.txt\nFull output sha256: cd15fb964c709e38041ea1ae54f12add71bf2c7ac8c8167883326629ccd06745\n</persisted-output>"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let replayed = recorded.replace(
            "/opt/asb-private/agent-state/home/.qwen/tmp/427298c01319355aae99da720b647df7cd291b9e3d2f3b5950ed35fb285aaf3f/tool-results/call_193d25ef253b4045a147311558349385.txt",
            "/opt/asb-private/agent-state/home/.qwen/tmp/427298c01319355aae99da720b647df7cd291b9e3d2f3b5950ed35fb285aaf3f/tool-results/asb-replay-1891570-<asb-timestamp>-4.txt",
        );
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_digest = replayed.replace(
            "cd15fb964c709e38041ea1ae54f12add71bf2c7ac8c8167883326629ccd06745",
            "dd15fb964c709e38041ea1ae54f12add71bf2c7ac8c8167883326629ccd06745",
        );
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_digest).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_openjiuwen_generated_bash_output_filename() {
        let root = test_root();
        let cassette = root.join("openjiuwen-bash-output.jsonl");
        let recorded = r#"{"model":"m","messages":[{"role":"tool","content":"Bash output saved to bash_outputs/bash_2ea5c8b331cb.txt\nPreview (first 2 lines):\nsemantic result: resolved"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let replayed = recorded.replace("bash_2ea5c8b331cb.txt", "bash_a0865e8bb19c.txt");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_preview = replayed.replace("resolved", "failed");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_preview).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_structured_python_callable_address_rendering() {
        let root = test_root();
        let cassette = root.join("structured-python-callable-address.jsonl");
        let recorded = r#"{"model":"m","input":[{"type":"function_call_output","output":"pytest diagnostic: <function call_runtest_hook.<locals>.<lambda> at 0x7a20954180d0>\nsemantic result: 70 passed"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/responses", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let replayed = r#"{"model":"m","input":[{"type":"function_call_output","output":"pytest diagnostic: <function call_runtest_hook.<locals>.<lambda> at 0x7309033850d0>\nsemantic result: 70 passed"}]}"#;
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", replayed).contains("200 OK"));
        session.stop().unwrap();

        let semantic_change = replayed.replace("70 passed", "69 passed");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &semantic_change).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_chat_completion_python_callable_address_rendering() {
        let root = test_root();
        let cassette = root.join("chat-python-callable-address.jsonl");
        let recorded = r#"{"model":"m","messages":[{"role":"tool","content":"function = <function concatenate at 0x7cff052910f0>\nsemantic result: resolved"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let replayed = r#"{"model":"m","messages":[{"role":"tool","content":"function = <function concatenate at 0x74923cf2d0b0>\nsemantic result: resolved"}]}"#;
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/chat/completions", replayed).contains("200 OK"));
        session.stop().unwrap();

        let semantic_change = replayed.replace("resolved", "failed");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(
            request_to(&session, "/v1/chat/completions", &semantic_change).contains("409 Conflict")
        );
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_escaped_chat_callable_address_rendering() {
        let root = test_root();
        let cassette = root.join("escaped-chat-python-callable-address.jsonl");
        let recorded = r#"{"model":"m","messages":[{"role":"tool","content":"function = \\u003cfunction concatenate at 0x7cff052910f0\\u003e\\nsemantic result: resolved"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let replayed = r#"{"model":"m","messages":[{"role":"tool","content":"function = \\u003cfunction concatenate at 0x74923cf2d0b0\\u003e\\nsemantic result: resolved"}]}"#;
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/chat/completions", replayed).contains("200 OK"));
        session.stop().unwrap();

        let semantic_change = replayed.replace("resolved", "failed");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(
            request_to(&session, "/v1/chat/completions", &semantic_change).contains("409 Conflict")
        );
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_structured_python_object_address_rendering() {
        let root = test_root();
        let cassette = root.join("structured-python-object-address.jsonl");
        let recorded = r#"{"model":"m","input":[{"type":"function_call_output","output":"projection: <astropy.wcs.wcsapi.wrappers.sliced_wcs.SlicedLowLevelWCS object at 0x74bf5f748880>\nsemantic result: resolved"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/responses", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let replayed = r#"{"model":"m","input":[{"type":"function_call_output","output":"projection: <astropy.wcs.wcsapi.wrappers.sliced_wcs.SlicedLowLevelWCS object at 0x7028ccc89730>\nsemantic result: resolved"}]}"#;
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", replayed).contains("200 OK"));
        session.stop().unwrap();

        let semantic_object_change = replayed.replace("SlicedLowLevelWCS", "DifferentWCS");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(
            request_to(&session, "/v1/responses", &semantic_object_change).contains("409 Conflict")
        );
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_structured_hermes_verification_script_path() {
        let root = test_root();
        let cassette = root.join("structured-hermes-verification-script.jsonl");
        let recorded = r#"{"model":"m","input":[{"type":"function_call_output","output":"Temporary verification script: /opt/asb-private/agent-state/tmp/hermes-verify-nnolyulw.py\nAd-hoc verification passed."}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/responses", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let replayed = r#"{"model":"m","input":[{"type":"function_call_output","output":"Temporary verification script: /opt/asb-private/agent-state/tmp/hermes-verify-fkdeg5vs.py\nAd-hoc verification passed."}]}"#;
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", replayed).contains("200 OK"));
        session.stop().unwrap();

        let filename_change =
            replayed.replace("hermes-verify-fkdeg5vs.py", "hermes-check-fkdeg5vs.py");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &filename_change).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));

        let content_change = replayed.replace("verification passed", "verification failed");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &content_change).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_complete_testbed_search_block_ordering() {
        let root = test_root();
        let cassette = root.join("opencode-search-order.jsonl");
        let recorded = r#"{"model":"m","messages":[{"content":"Found 2 matches\n/testbed/alpha.py:\n  Line 1: first\n\n\n/testbed/beta.py:\n  Line 2: second\n"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let reordered = r#"{"model":"m","messages":[{"content":"Found 2 matches\n/testbed/beta.py:\n  Line 2: second\n\n\n/testbed/alpha.py:\n  Line 1: first\n"}]}"#;
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let response = request(&session, reordered);
        assert!(response.contains("200 OK"), "{response}");
        session.stop().unwrap();

        let changed_match = reordered.replace("Line 2: second", "Line 2: changed");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_match).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_pip_download_transport_progress() {
        let root = test_root();
        let cassette = root.join("pip-progress.jsonl");
        let recorded = r#"{"model":"m","messages":[{"content":"Downloading numpy-1.22.4.whl (16.8 MB)\n   ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ 16.8/16.8 MB 17.6 MB/s eta 0:00:00"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let replayed = r#"{"model":"m","messages":[{"content":"Downloading numpy-1.22.4.whl (16.8 MB)\n   ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ 16.8/16.8 MB 23.0 MB/s eta 0:00:00"}]}"#;
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_artifact = replayed.replace("16.8/16.8 MB", "17.0/17.0 MB");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_artifact).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_ls_stderr_interleaving() {
        let root = test_root();
        let cassette = root.join("ls-stderr.jsonl");
        let recorded = r#"{"model":"m","messages":[{"content":"ls: cannot access '/opt/venv': No such file or directory\n/opt:\n asb-private\n/testbed:\n wcs.py\n"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let replayed = r#"{"model":"m","messages":[{"content":"/opt:\n asb-private\n/testbed:\n wcs.py\nls: cannot access '/opt/venv': No such file or directory\n"}]}"#;
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_diagnostic = replayed.replace("No such file or directory", "Permission denied");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_diagnostic).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_pytest_elapsed_time_with_all_count_categories() {
        let root = test_root();
        let cassette = root.join("pytest-summary.jsonl");
        let recorded = r#"{"model":"m","messages":[{"content":"120 passed, 7 skipped, 48 deselected, 6 errors in 1.44s"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let replayed = r#"{"model":"m","messages":[{"content":"120 passed, 7 skipped, 48 deselected, 6 errors in 1.65s"}]}"#;
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_counts = replayed.replace("6 errors", "7 errors");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_counts).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_single_pytest_pass_summary_after_structured_canonicalization() {
        let root = test_root();
        let cassette = root.join("pytest-single-summary.jsonl");
        let recorded = r#"{"model":"m","messages":[{"content":"Internet access disabled\n......................................................................   [100%]\n70 passed in 0.38s\n","role":"tool"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let replayed = recorded.replace("0.38s", "0.37s");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let response = request(&session, &replayed);
        let stop = session.stop();
        assert!(response.contains("200 OK"), "{response}; {stop:?}");
        stop.unwrap();

        let changed_count = replayed.replace("70 passed", "71 passed");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_count).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_codex_shell_envelope_wall_time() {
        let root = test_root();
        let cassette = root.join("codex-shell-wall-time.jsonl");
        let recorded = r#"{"model":"m","input":[{"type":"function_call_output","output":"Chunk ID: <asb-chunk>\nWall time: 0.2164 seconds\nProcess exited with code 0\nOriginal token count: 12\nresult"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/responses", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();

        let replayed = recorded.replace("0.2164", "0.1995");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_exit = replayed.replace("code 0", "code 1");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &changed_exit).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_codex_mcp_result_render_time() {
        let root = test_root();
        let cassette = root.join("codex-mcp-result-time.jsonl");
        let recorded = r#"{"model":"m","input":[{"type":"function_call_output","output":"Wall time: 0.2164 seconds\nOutput:\n{\"sealed\":true}"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/responses", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();

        let replayed = recorded.replace("0.2164", "0.1995");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_metadata = replayed.replace("true", "false");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &changed_metadata).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_openhands_generated_conversation_hierarchy() {
        let root = test_root();
        let cassette = root.join("openhands-conversation.jsonl");
        let recorded = r#"{"model":"m","input":[{"type":"function_call_output","output":[{"text":"saved /opt/asb-private/openhands/conversations/dc6652d07c19492ebea95849/subagents/b00423c915b64ed7a901"}]}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/responses", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();

        let replayed = recorded
            .replace("dc6652d07c19492ebea95849", "c872afba840b4e508b9e3a98")
            .replace("b00423c915b64ed7a901", "fd24f47432974e918a6c4b95");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_parent = replayed.replace("openhands", "other-hands");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &changed_parent).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));

        let non_hex = recorded.replace("b00423c915b64ed7a901", "user-selected");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &non_hex).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_openhands_terminal_filename_and_pytest_elapsed_renderer() {
        let root = test_root();
        let cassette = root.join("openhands-terminal-pytest.jsonl");
        let recorded = r#"{"model":"m","input":[{"type":"function_call_output","output":[{"text":"saved /opt/asb-private/openhands/conversations/<asb-openhands-conversation>/observations/terminal_output_c435ed15.txt\n\u001b[32m6 failed, 48 deselected\u001b[0m\u001b[32m in 0.41s\u001b[0m\nfinal result"}]}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/responses", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();

        let replayed = recorded
            .replace("c435ed15", "c01585f6")
            .replace("0.41s", "0.40s");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_count = replayed.replace("48 deselected", "47 deselected");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &changed_count).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));

        let changed_final = replayed.replace("final result", "changed final result");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &changed_final).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));

        let non_summary = replayed.replace("6 failed, 48 deselected", "native progress");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &non_summary).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_openjiuwen_ephemeral_shell_path_component() {
        let root = test_root();
        let cassette = root.join("openjiuwen-shell-path.jsonl");
        let recorded = r#"{"model":"m","messages":[{"content":[{"type":"text","text":"saved /tmp/agent/artifacts/bash_eb4149008"}]}],"temperature":0}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/messages", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();

        let replayed = recorded.replace("eb4149008", "6c88783c4");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/messages", &replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_text = replayed.replace("artifacts", "changed-artifacts");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/messages", &changed_text).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_structured_pytest_callable_address_renderer() {
        let root = test_root();
        let cassette = root.join("pytest-callable-address.jsonl");
        // Escaped angle brackets ensure the raw-string normalizer cannot see
        // this renderer; the structured string renderer is what is covered.
        let recorded = r#"{"model":"m","messages":[{"content":"func = \u003cfunction call_runtest_hook.\u003clocals\u003e.\u003clambda\u003e at 0x7b230e429940\u003e\nwhen = call"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();

        let replayed = recorded.replace("7b230e429940", "7d1484ce9940");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_callable = replayed.replace("call_runtest_hook", "call_other_hook");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_callable).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));

        let changed_final = replayed.replace("when = call", "when = setup");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_final).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_structured_pytest_testmaps_address_renderer() {
        let root = test_root();
        let cassette = root.join("pytest-testmaps-address.jsonl");
        let recorded = r#"{"model":"m","messages":[{"content":"self = \u003castropy.wcs.tests.test_wcs.TestMaps object at 0x7b230ebc51c0\u003e\nrequest = call"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();

        let replayed = recorded.replace("7b230ebc51c0", "714a1dfbd670");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_class = replayed.replace("TestMaps", "OtherMaps");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_class).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));

        let changed_final = replayed.replace("request = call", "request = setup");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_final).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_opencode_completed_task_session_id() {
        let root = test_root();
        let cassette = root.join("opencode-task-session.jsonl");
        let recorded = r#"{"model":"m","messages":[{"content":"<task id=\"ses_record_123\" state=\"completed\">result</task>"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();

        let replayed = recorded.replace("ses_record_123", "ses_replay_456");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_state = replayed.replace("completed", "failed");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed_state).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_only_codex_async_shell_envelope() {
        let root = test_root();
        let cassette = root.join("codex-async-shell.jsonl");
        let recorded = r#"{"model":"m","input":[{"type":"function_call_output","output":"Chunk ID: deadbeef\nWall time: 0.2164 seconds\nProcess running with session ID 7812\nwaiting for command output"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/responses", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();

        let replayed = recorded.replace("0.2164", "1.904").replace("7812", "9914");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_status = replayed.replace("Process running", "Process exited");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &changed_status).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));

        let changed_output = replayed.replace("waiting for command output", "different output");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", &changed_output).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_rejects_changed_body_and_extra_requests() {
        let root = test_root();
        let cassette = cassette(&root);
        let workspace = Path::new("/tmp/replay-workspace");
        let session = Session::start(Mode::Replay, &cassette, None, false, workspace).unwrap();
        let changed =
            r#"{"model":"m","messages":[{"content":"different"}],"tools":[{"type":"function"}]}"#;
        assert!(request(&session, changed).contains("409 Conflict"));
        let error = session.stop().unwrap_err().to_string();
        assert!(error.contains("rejected"), "{error}");
        assert!(
            error.contains("json_value at $.messages[0].content"),
            "{error}"
        );
        let session = Session::start(Mode::Replay, &cassette, None, false, workspace).unwrap();
        let valid = r#"{"model":"m","messages":[{"content":"work in /tmp/replay-workspace"}],"tools":[{"type":"function"}]}"#;
        assert!(request(&session, valid).contains("200 OK"));
        assert!(request(&session, valid).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_accepts_equal_null_fields_and_rejects_a_changed_null() {
        let root = test_root();
        let cassette = root.join("null-fields.jsonl");
        let recorded = r#"{"model":"m","messages":[{"role":"assistant","content":null}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body": recorded},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();

        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, recorded).contains("200 OK"));
        session.stop().unwrap();

        let changed = recorded.replace("null", "\"changed\"");
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, &changed).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_progress_redacts_string_values_but_reports_shape() {
        let root = test_root();
        let cassette = cassette(&root);
        let workspace = Path::new("/tmp/replay-workspace");
        let session = Session::start(Mode::Replay, &cassette, None, false, workspace).unwrap();
        let changed =
            r#"{"model":"m","messages":[{"content":"different"}],"tools":[{"type":"function"}]}"#;
        assert!(request(&session, changed).contains("409 Conflict"));
        let progress = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("cassette.jsonl.progress.json"))
            .find(|path| path.is_file())
            .unwrap();
        let text = std::fs::read_to_string(progress).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["first_mismatch"]["string_lengths"],
            serde_json::json!({"expected": 23, "actual": 9})
        );
        assert_eq!(value["first_mismatch"]["first_difference_offset"], 0);
        assert!(!text.contains("different"));
        assert!(!text.contains("work in /tmp/replay-workspace"));
        assert!(session.stop().is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_preserves_distinct_tool_call_ids_in_a_response() {
        let root = test_root();
        let cassette = root.join("tool-calls.jsonl");
        let request_body = r#"{"model":"m","messages":[{"role":"user","content":"work"}]}"#;
        let response_body = r#"{"tool_calls":[{"id":"call_<asb-call-1>"},{"id":"call_<asb-call-2>"},{"id":"call_<asb-call-1>"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body":request_body},
            "response": {"status":200, "headers":{"content-type":"application/json"}, "body":response_body}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let response = request(&session, request_body);
        let ids = response
            .split("call_asb-replay-")
            .skip(1)
            .map(|value| value.split('"').next().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 3, "{response}");
        assert_eq!(ids[0], ids[2], "{response}");
        assert_ne!(ids[0], ids[1], "{response}");
        session.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_materializes_structured_identity_markers_in_responses() {
        let root = test_root();
        let cassette = root.join("structured-identity-response.jsonl");
        let request_body = r#"{"model":"m","messages":[{"role":"user","content":"work"}]}"#;
        let response_body = r#"{"tool_calls":[{"id":"<asb-tool-call-1>"}],"content":[{"id":"<asb-content-block-1>"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body":request_body},
            "response": {"status":200, "headers":{"content-type":"application/json"}, "body":response_body}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let response = request(&session, request_body);
        assert!(response.contains("asb-replay-"), "{response}");
        assert!(!response.contains("<asb-tool-call-1>"), "{response}");
        assert!(!response.contains("<asb-content-block-1>"), "{response}");
        session.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_preserves_materialized_protocol_ids_in_later_history() {
        let root = test_root();
        let cassette = root.join("materialized-history.jsonl");
        let initial = r#"{"model":"m","messages":[{"role":"user","content":"work"}]}"#;
        let history = r#"{"model":"m","messages":[{"role":"assistant","tool_calls":[{"id":"<asb-tool-call-1>","type":"function","function":{"name":"read","arguments":"{}"}},{"id":"<asb-tool-call-2>","type":"function","function":{"name":"write","arguments":"{}"}}]},{"role":"tool","tool_call_id":"<asb-tool-call-2>","content":"done"}]}"#;
        let first = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body":initial},
            "response": {"status":200, "headers":{"content-type":"application/json"}, "body":"{\"tool_calls\":[{\"id\":\"<asb-tool-call-1>\"},{\"id\":\"<asb-tool-call-2>\"}]}"}
        });
        let second = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body":history},
            "response": {"status":200, "headers":{"content-type":"application/json"}, "body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{first}\n{second}\n")).unwrap();
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let response = request(&session, initial);
        let ids = response
            .split("asb-replay-")
            .skip(1)
            .map(|suffix| format!("asb-replay-{}", suffix.split('"').next().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 2, "{response}");
        assert_ne!(ids[0], ids[1], "{response}");
        let replayed_history = history
            .replace("<asb-tool-call-1>", &ids[0])
            .replace("<asb-tool-call-2>", &ids[1]);
        assert!(request(&session, &replayed_history).contains("200 OK"));
        session.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_opaque_openai_tool_call_ids_in_message_history() {
        let root = test_root();
        let cassette = root.join("opaque-tool-calls.jsonl");
        let recorded = r#"{"model":"m","messages":[{"role":"assistant","tool_calls":[{"id":"call_11111111-1111-1111-1111-111111111111","type":"function","function":{"name":"read_file","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_11111111-1111-1111-1111-111111111111","content":"done"}]}"#;
        let replayed = r#"{"model":"m","messages":[{"role":"assistant","tool_calls":[{"id":"22222222-2222-2222-2222-222222222222","type":"function","function":{"name":"read_file","arguments":"{}"}}]},{"role":"tool","tool_call_id":"22222222-2222-2222-2222-222222222222","content":"done"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body":recorded},
            "response": {"status":200, "headers":{"content-type":"application/json"}, "body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, replayed).contains("200 OK"));
        session.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_legacy_tape_tool_call_markers_structurally() {
        let root = test_root();
        let cassette = root.join("legacy-tool-call-marker.jsonl");
        let recorded = r#"{"model":"m","messages":[{"role":"assistant","tool_calls":[{"id":"<asb-call-2>","type":"function","function":{"name":"read_file","arguments":"{}"}}]},{"role":"tool","tool_call_id":"<asb-call-2>","content":"done"}]}"#;
        let replayed = r#"{"model":"m","messages":[{"role":"assistant","tool_calls":[{"id":"call_33333333-3333-3333-3333-333333333333","type":"function","function":{"name":"read_file","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_33333333-3333-3333-3333-333333333333","content":"done"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/chat/completions", "body":recorded},
            "response": {"status":200, "headers":{"content-type":"application/json"}, "body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, replayed).contains("200 OK"));
        session.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_normalizes_anthropic_content_tool_ids() {
        let root = test_root();
        let cassette = root.join("adapter-volatile-ids.jsonl");
        let recorded = r#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01ARecord","name":"read_file","input":{"path":"src/lib.rs"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01ARecord","content":"done"}]}],"temperature":0}"#;
        let replayed = r#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01BReplay","name":"read_file","input":{"path":"src/lib.rs"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01BReplay","content":"done"}]}],"temperature":0}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/messages", "body":recorded},
            "response": {"status":200, "headers":{"content-type":"application/json"}, "body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let response = request_to(&session, "/v1/messages", replayed);
        if !response.contains("200 OK") {
            let error = session.stop().unwrap_err();
            panic!("{response}\n{error}");
        }
        session.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_rejects_oversized_request_during_streaming() {
        let root = test_root();
        let cassette = cassette(&root);
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let response = request(&session, &"x".repeat(16 * 1024 * 1024 + 1));
        assert!(response.contains("413 Payload Too Large"), "{response}");
        assert!(response.contains("request body exceeds 16 MiB"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_rejects_omitted_tool_turns_and_query_mutations() {
        let root = test_root();
        let cassette = root.join("responses.jsonl");
        let expected = r#"{"model":"m","input":[{"role":"user","content":"work"},{"type":"function_call_output","call_id":"old","output":"done"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/responses", "body": expected},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"call_<asb-call>\":\"call_<asb-call>\",\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let omitted_tool_turn =
            r#"{"model":"m","input":[{"role":"user","content":"work"}],"temperature":0.2}"#;
        assert!(request_to(&session, "/v1/responses", omitted_tool_turn).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));

        // A query can influence provider semantics. It must not be discarded
        // just because the path and the initial user item happen to match.
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(
            request_to(&session, "/v1/responses?api-version=2025-01-01", expected)
                .contains("409 Conflict")
        );
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));

        // The whole original request remains replayable, including the tool
        // result. The proxy never fabricates or elides a local tool turn.
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/responses", expected).contains("200 OK"));
        session.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_skips_openjiuwen_vision_capability_probe() {
        let root = test_root();
        let cassette = root.join("probe.jsonl");
        let embedding = r#"{"model":"text-embedding-3-small","input":["memory"]}"#;
        // The recorder canonicalizes an omitted Anthropic temperature to 0,
        // so the cassette uses the same normalized wire request that the
        // replay proxy will compare.
        let probe = r#"{"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"What color is this image? Reply with one word."}]}],"temperature":0}"#;
        let workload =
            r#"{"model":"m","messages":[{"role":"user","content":"work"}],"temperature":0}"#;
        let interaction = |body: &str| {
            serde_json::json!({
                "request": {"method":"POST", "url":"/v1/messages", "body":body},
                "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
            })
        };
        let embedding_interaction = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/embeddings", "body":embedding},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":"{\"ok\":true}"}
        });
        std::fs::write(
            &cassette,
            format!(
                "{}\n{}\n{}\n",
                embedding_interaction,
                interaction(probe),
                interaction(workload)
            ),
        )
        .unwrap();
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request_to(&session, "/v1/messages", probe).contains("200 OK"));
        assert!(request_to(&session, "/v1/messages", workload).contains("200 OK"));
        session.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn record_rejects_oversized_upstream_response_during_streaming() {
        let root = test_root();
        let cassette = root.join("record.jsonl");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            connection
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            connection
                .set_write_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut incoming = [0u8; 8192];
            assert!(connection.read(&mut incoming).unwrap() > 0);
            connection
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 16777217\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            let chunk = vec![b'x'; 65536];
            for _ in 0..256 {
                if connection.write_all(&chunk).is_err() {
                    return;
                }
            }
            let _ = connection.write_all(b"x");
        });
        let session =
            Session::start(Mode::Record, &cassette, Some(&upstream), false, &root).unwrap();
        let response = request(&session, "{}");
        assert!(response.contains("502 Bad Gateway"), "{response}");
        assert!(response.contains("upstream response body exceeds 16 MiB"));
        session.stop().unwrap();
        server.join().unwrap();
        assert!(!cassette.exists() || std::fs::metadata(&cassette).unwrap().len() == 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn records_then_replays_with_a_different_workspace() {
        let root = test_root();
        let cassette = root.join("record.jsonl");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut request = [0u8; 8192];
            let _ = connection.read(&mut request).unwrap();
            connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
        });
        let session = Session::start(
            Mode::Record,
            &cassette,
            Some(&upstream),
            false,
            Path::new("/tmp/original-workspace"),
        )
        .unwrap();
        let original = r#"{"model":"m","messages":[{"content":"work in /tmp/original-workspace"}],"tools":[{"type":"function"}]}"#;
        assert!(request(&session, original).contains("200 OK"));
        let completed = session.finish();
        completed.result.unwrap();
        assert!(completed.evidence.cleanup_confirmed);
        assert_eq!(completed.evidence.captures.len(), 1);
        let capture = &completed.evidence.captures[0];
        assert!(capture.complete);
        assert_eq!(capture.inference_requests, 1);
        assert_eq!(capture.output, cassette);
        assert_eq!(capture.bytes, std::fs::metadata(&cassette).unwrap().len());
        server.join().unwrap();
        // Even if a caller supplies an online endpoint for replay, no hop
        // receives it and the recorded response is served offline.
        let forbidden = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        forbidden.set_nonblocking(true).unwrap();
        let forbidden_url = format!("http://{}", forbidden.local_addr().unwrap());
        let session = Session::start(
            Mode::Replay,
            &cassette,
            Some(&forbidden_url),
            false,
            Path::new("replay-sweep-results/000-record/workspace-concurrency-1.asb-agent-runtime"),
        )
        .unwrap();
        let replay = r#"{"model":"m","messages":[{"content":"work in replay-sweep-results/000-record/workspace-concurrency-1.asb-agent-runtime"}],"tools":[{"type":"function"}]}"#;
        assert!(request(&session, replay).contains("200 OK"));
        session.stop().unwrap();
        assert_eq!(
            forbidden.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_ignores_volatile_claude_user_metadata() {
        let root = test_root();
        let cassette = root.join("record.jsonl");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut request = [0u8; 8192];
            let _ = connection.read(&mut request).unwrap();
            connection.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            ).unwrap();
        });
        let recorded = r#"{"model":"m","metadata":{"user_id":"{\"device_id\":\"recorded\",\"session_id\":\"<asb-uuid>\"}"},"messages":[{"content":{"device_id":"recorded","text":"work"}}]}"#;
        let session =
            Session::start(Mode::Record, &cassette, Some(&upstream), false, &root).unwrap();
        assert!(request(&session, recorded).contains("200 OK"));
        session.stop().unwrap();
        server.join().unwrap();

        let replayed = r#"{"model":"m","metadata":{"user_id":"{\"device_id\":\"replayed\",\"session_id\":\"<asb-uuid>\"}"},"messages":[{"content":{"device_id":"recorded","text":"work"}}]}"#;
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, replayed).contains("200 OK"));
        session.stop().unwrap();

        let changed_outside_metadata = r#"{"model":"m","metadata":{"user_id":"{\"device_id\":\"replayed-again\",\"session_id\":\"<asb-uuid>\"}"},"messages":[{"content":{"device_id":"changed","text":"work"}}]}"#;
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        assert!(request(&session, changed_outside_metadata).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_ignores_only_known_volatile_codex_client_metadata() {
        let root = test_root();
        let cassette = root.join("codex-client-metadata.jsonl");
        let recorded = r#"{"model":"m","client_metadata":{"root_turn_id":"recorded-root","session_id":"recorded-session","thread_id":"recorded-thread","turn_id":"recorded-turn","x-codex-installation-id":"recorded-install","x-codex-turn-metadata":"recorded-telemetry","x-codex-window-id":"recorded-window"},"input":[{"role":"user","content":"work"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/responses", "body": recorded},
            "response": {"status":200, "headers":{"content-type":"application/json"}, "body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();
        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let replayed = r#"{"model":"m","client_metadata":{"root_turn_id":"replayed-root","session_id":"replayed-session","thread_id":"replayed-thread","turn_id":"replayed-turn","x-codex-installation-id":"replayed-install","x-codex-turn-metadata":"replayed-telemetry","x-codex-window-id":"replayed-window"},"input":[{"role":"user","content":"work"}]}"#;
        assert!(request_to(&session, "/v1/responses", replayed).contains("200 OK"));
        session.stop().unwrap();

        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let changed_content = replayed.replace("\"work\"", "\"changed\"");
        assert!(request_to(&session, "/v1/responses", &changed_content).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_ignores_only_opaque_codex_input_encrypted_content() {
        let root = test_root();
        let cassette = root.join("codex-encrypted-content.jsonl");
        let recorded = r#"{"model":"m","input":[{"type":"function_call","name":"shell","arguments":"{}","encrypted_content":"recorded-opaque"},{"role":"user","content":"work"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/responses", "body": recorded},
            "response": {"status":200, "headers":{"content-type":"application/json"}, "body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();

        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let replayed = r#"{"model":"m","input":[{"type":"function_call","name":"shell","arguments":"{}","encrypted_content":{"new":"opaque"}},{"role":"user","content":"work"}]}"#;
        assert!(request_to(&session, "/v1/responses", replayed).contains("200 OK"));
        session.stop().unwrap();

        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let changed_input = replayed.replace("\"work\"", "\"changed\"");
        assert!(request_to(&session, "/v1/responses", &changed_input).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_ignores_only_opaque_codex_input_call_id() {
        let root = test_root();
        let cassette = root.join("codex-input-call-id.jsonl");
        let recorded = r#"{"model":"m","input":[{"type":"function_call_output","call_id":"recorded-call","output":"fixed tool result"},{"role":"user","content":"work"}]}"#;
        let line = serde_json::json!({
            "request": {"method":"POST", "url":"/v1/responses", "body": recorded},
            "response": {"status":200, "headers":{"content-type":"application/json"}, "body":"{\"ok\":true}"}
        });
        std::fs::write(&cassette, format!("{line}\n")).unwrap();

        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let replayed = r#"{"model":"m","input":[{"type":"function_call_output","call_id":"replayed-generated-call-id-with-a-different-shape","output":"fixed tool result"},{"role":"user","content":"work"}]}"#;
        assert!(request_to(&session, "/v1/responses", replayed).contains("200 OK"));
        session.stop().unwrap();

        let session = Session::start(Mode::Replay, &cassette, None, false, &root).unwrap();
        let changed_tool_result = replayed.replace("fixed tool result", "changed tool result");
        assert!(
            request_to(&session, "/v1/responses", &changed_tool_result).contains("409 Conflict")
        );
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_can_honor_recorded_response_delay() {
        let root = test_root();
        let cassette = cassette(&root);
        let line = std::fs::read_to_string(&cassette).unwrap();
        let mut interaction: serde_json::Value = serde_json::from_str(&line).unwrap();
        interaction["delay_ms"] = 200.into();
        std::fs::write(&cassette, format!("{interaction}\n")).unwrap();
        let session = Session::start(
            Mode::Replay,
            &cassette,
            None,
            true,
            Path::new("/tmp/replay-workspace"),
        )
        .unwrap();
        let valid = r#"{"model":"m","messages":[{"content":"work in /tmp/replay-workspace"}],"tools":[{"type":"function"}]}"#;
        let started = std::time::Instant::now();
        assert!(request(&session, valid).contains("200 OK"));
        assert!(started.elapsed() >= std::time::Duration::from_millis(180));
        session.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn built_in_proxy_is_materialized_beside_the_cassette() {
        let root = std::env::temp_dir().join(format!("asb-tape-test-{}", std::process::id()));
        let cassette = root.join("cassettes/run.jsonl");
        std::fs::create_dir_all(cassette.parent().unwrap()).unwrap();
        let resolved = resolve_executable(cassette.parent().unwrap()).unwrap();
        match resolved {
            ProxyExecutable::Builtin(path) => {
                assert_eq!(path, root.join("cassettes/.asb-tape-proxy.mjs"));
                assert!(
                    std::fs::read_to_string(path)
                        .unwrap()
                        .contains("http.createServer")
                );
            }
            ProxyExecutable::Override(_) => panic!("expected the embedded tape proxy"),
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_proxy_override_remains_supported() {
        // Environment changes are process-global, so exercise the command shape
        // directly rather than changing ASB_LLM_TAPE in a parallel test.
        let command = ProxyExecutable::Override("custom-tape".into()).command();
        assert_eq!(command.get_program(), "custom-tape");
    }

    #[test]
    fn proxy_chain_is_ordered_from_agent_to_provider() {
        assert_eq!(
            parse_chain("llmtape, audit, redact").unwrap(),
            vec!["llmtape", "audit", "redact"]
        );
        assert!(parse_chain(" , ").is_err());
    }
}
