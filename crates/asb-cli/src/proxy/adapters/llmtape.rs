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
    url: String,
    progress: Option<PathBuf>,
    cassette: PathBuf,
    mode: Mode,
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

    fn start(
        &self,
        upstream: Option<&str>,
        settings: Option<&dyn Any>,
    ) -> Result<Box<dyn ProxySession>, Error> {
        let request = settings
            .and_then(|settings| settings.downcast_ref::<Settings>())
            .ok_or_else(|| Error::Input("llmtape requires record/replay settings".into()))?;
        let cassette = &request.cassette;
        if request.mode == Mode::Record {
            let parent = cassette
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::Run(format!(
                    "cannot create cassette directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
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
        // Hold the startup lock until the proxy is listening so sibling
        // replicas do not select the same released ephemeral port.
        let _start_guard = START_LOCK
            .lock()
            .map_err(|_| Error::Run("tape startup lock was poisoned".into()))?;
        let port = free_port()?;
        let executable = resolve_executable(&scratch.path)?;
        let mut command = executable.command();
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
        command
            .env("ASB_TAPE_WORKSPACE", &request.workspace)
            .stdin(Stdio::null())
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
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
                return Ok(Box::new(RunningLlmtape {
                    child: child.0.take(),
                    url: format!("http://127.0.0.1:{port}/v1"),
                    progress: (request.mode == Mode::Replay)
                        .then(|| replay_cassette.with_extension("jsonl.progress.json")),
                    cassette: cassette.clone(),
                    mode: request.mode,
                    _scratch: scratch,
                }));
            }
            if child
                .0
                .as_mut()
                .expect("startup child is present")
                .try_wait()
                .map_err(|error| Error::Run(format!("cannot inspect tape proxy: {error}")))?
                .is_some()
            {
                return Err(Error::Run("ASB's tape proxy exited before becoming ready; verify Node.js and the cassette format".into()));
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(Error::Run(
            "ASB's tape proxy did not become ready within 8 seconds; verify Node.js and the cassette format".into(),
        ))
    }
}

impl ProxySession for RunningLlmtape {
    fn url(&self) -> &str {
        &self.url
    }

    fn finish(&mut self) -> Result<(), Error> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        crate::project::stop_network_child(&mut child)
            .map_err(|error| Error::Run(format!("could not stop proxy adapter: {error}")))?;
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
                return Err(Error::Run(format!(
                    "replay rejected {rejected} request(s) for {}; the normalized request or sequence differs from the recording",
                    self.cassette.display()
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
            let _ = crate::project::stop_network_child(&mut child);
        }
    }
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
    if let Some(override_path) = std::env::var("ASB_LLM_TAPE")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(ProxyExecutable::Override(override_path));
    }
    let script = directory.join(".asb-tape-proxy.mjs");
    std::fs::write(directory.join("network-fetch.mjs"), NETWORK_FETCH)
        .map_err(|error| Error::Run(format!("cannot materialize tape network client: {error}")))?;
    std::fs::write(&script, BUILTIN_PROXY).map_err(|error| {
        Error::Run(format!(
            "cannot materialize ASB's built-in tape proxy at {}: {error}",
            script.display()
        ))
    })?;
    Ok(ProxyExecutable::Builtin(script))
}

// Created exclusively, so teardown only removes files owned by this session.
struct Scratch {
    path: PathBuf,
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
                Ok(()) => return Ok(Self { path }),
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
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn free_port() -> Result<u16, Error> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map(|listener| listener.local_addr().map(|address| address.port()))
        .map_err(|error| Error::Run(format!("cannot reserve a local llmtape port: {error}")))?
        .map_err(|error| Error::Run(format!("cannot inspect local llmtape port: {error}")))
}

#[cfg(test)]
mod tests {
    use super::{ProxyExecutable, resolve_executable};
    use crate::proxy::parse_chain;
    use crate::tape::{Mode, Session};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

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
        write!(socket, "POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
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
    fn replay_rejects_changed_body_and_extra_requests() {
        let root = test_root();
        let cassette = cassette(&root);
        let workspace = Path::new("/tmp/replay-workspace");
        let session = Session::start(Mode::Replay, &cassette, None, false, workspace).unwrap();
        let changed =
            r#"{"model":"m","messages":[{"content":"different"}],"tools":[{"type":"function"}]}"#;
        assert!(request(&session, changed).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
        let session = Session::start(Mode::Replay, &cassette, None, false, workspace).unwrap();
        let valid = r#"{"model":"m","messages":[{"content":"work in /tmp/replay-workspace"}],"tools":[{"type":"function"}]}"#;
        assert!(request(&session, valid).contains("200 OK"));
        assert!(request(&session, valid).contains("409 Conflict"));
        assert!(session.stop().unwrap_err().to_string().contains("rejected"));
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
        session.stop().unwrap();
        server.join().unwrap();
        let session = Session::start(
            Mode::Replay,
            &cassette,
            None,
            false,
            Path::new("replay-sweep-results/000-record/workspace-concurrency-1"),
        )
        .unwrap();
        let replay = r#"{"model":"m","messages":[{"content":"work in replay-sweep-results/000-record/workspace-concurrency-1"}],"tools":[{"type":"function"}]}"#;
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
