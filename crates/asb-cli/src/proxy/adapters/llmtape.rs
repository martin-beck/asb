//! Local llmtape proxy lifecycle for deterministic LLM recording and replay.

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

pub(crate) const NAME: &str = "llmtape";

pub(crate) struct Settings {
    pub(crate) mode: Mode,
    pub(crate) cassette: PathBuf,
    pub(crate) respect_record_timings: bool,
    pub(crate) workspace: PathBuf,
}

struct RunningLlmtape {
    child: Option<Child>,
    builtin: bool,
    url: String,
    progress: Option<PathBuf>,
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
        let scratch = Scratch::create(parent)?;
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
        if process
            .try_wait()
            .map_err(|error| Error::Run(format!("cannot inspect tape proxy: {error}")))?
            .is_some()
        {
            return Err(Error::Run("ASB's tape proxy exited before becoming ready; verify Node.js and the cassette format".into()));
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
                return Err(Error::Run(format!(
                    "replay rejected {rejected} request(s) for {}{mismatch}; the normalized request or sequence differs from the recording",
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
        let session = Session::start(
            Mode::Replay,
            &cassette,
            None,
            false,
            Path::new("replay-sweep-results/000-record/workspace-concurrency-1.asb-agent-runtime"),
        )
        .unwrap();
        let replay = r#"{"model":"m","messages":[{"content":"work in replay-sweep-results/000-record/workspace-concurrency-1.asb-agent-runtime"}],"tools":[{"type":"function"}]}"#;
        assert!(request(&session, replay).contains("200 OK"));
        session.stop().unwrap();
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
