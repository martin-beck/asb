//! Lifecycle wrapper around the project-local `http-server` tool.
//!
//! ASB materializes an allowlisted publication directory, then delegates HTTP
//! protocol handling to the established `http-server` package. The wrapper
//! owns only port selection, per-project state, idle expiry and process
//! termination; it never exposes the project directory itself.

use crate::{Error, project, tools};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
#[cfg(test)]
use std::time::SystemTime;
use std::time::{Duration, Instant, UNIX_EPOCH};

const STATE_NAME: &str = "http-server.json";
const PUBLICATION_DIR: &str = "http-publication";
const IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const START_TIMEOUT: Duration = Duration::from_secs(3);
const STARTUP_BUDGET: Duration = Duration::from_secs(27);
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 10_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct State {
    schema_version: u8,
    #[serde(default)]
    generation: Option<String>,
    port: u16,
    public_host: String,
    executable: PathBuf,
    publication: PathBuf,
    traces: Vec<PathBuf>,
    pid: Option<u32>,
    #[serde(default)]
    control: Option<Control>,
    #[serde(default)]
    owner_identity: Option<ProcessIdentity>,
    #[serde(default)]
    server_identity: Option<ProcessIdentity>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ProcessIdentity {
    pid: u32,
    boot_id: String,
    start_time: u64,
    #[serde(default)]
    pid_namespace: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Control {
    port: u16,
    token: String,
}

/// URLs served by this project's current report server.
pub(crate) struct ServerInfo {
    base: Option<String>,
    traces: Vec<PathBuf>,
}
impl ServerInfo {
    pub(crate) fn report_url(&self, report: &Path) -> String {
        self.base
            .as_ref()
            .map(|base| format!("{base}/report.html"))
            .unwrap_or_else(|| local_file_url(report))
    }
    pub(crate) fn trace_url(&self, trace: &Path) -> Option<String> {
        let trace = trace.canonicalize().ok()?;
        self.traces
            .iter()
            .position(|item| item == &trace)
            .and_then(|index| {
                self.base
                    .as_ref()
                    .map(|base| format!("{base}/traces/{index}.{}", trace_extension(&trace)))
            })
    }
    pub(crate) fn traces(&self) -> &[PathBuf] {
        &self.traces
    }
}

pub(crate) fn start_for_report(
    report: &Path,
    traces: Vec<PathBuf>,
    output: &mut dyn Write,
) -> Result<ServerInfo, Error> {
    let (root, config) = project::load(report)?;
    if tools::resolve("http", &root, &config.tools_dir).is_none() {
        return Ok(ServerInfo {
            base: None,
            traces: canonical_traces(traces),
        });
    }
    start(&root, &config, report, traces, output)
}

pub(crate) fn start_manual(output: &mut dyn Write) -> Result<(), Error> {
    let (root, config) = project::load(Path::new("."))?;
    let (report, traces) = latest_report(&root)?;
    let server = start(&root, &config, &report, traces, output)?;
    writeln!(
        output,
        "HTTP report server: {}\nServes only the generated report and its collector traces; stops one hour after the last request.",
        server.report_url(&report)
    )
    .map_err(crate::io_error)
}

pub(crate) fn stop_manual(output: &mut dyn Write) -> Result<(), Error> {
    let (root, _) = project::load(Path::new("."))?;
    let _owner = lock_project(&root)?;
    let path = state_path(&root);
    let state = read_state(&path).map_err(|_| {
        Error::Input("no ASB HTTP report server is running for this project".into())
    })?;
    stop(&path, &state)?;
    writeln!(output, "Stopped ASB HTTP report server.").map_err(crate::io_error)
}

fn start(
    root: &Path,
    config: &project::ProjectConfig,
    report: &Path,
    traces: Vec<PathBuf>,
    _output: &mut dyn Write,
) -> Result<ServerInfo, Error> {
    let report = report.canonicalize().map_err(crate::io_error)?;
    if report.file_name().and_then(|name| name.to_str()) != Some("report.html") {
        return Err(Error::Input(
            "only generated report.html files may be served".into(),
        ));
    }
    let traces = canonical_traces(traces);
    if cfg!(test) {
        return Ok(ServerInfo {
            base: Some("http://203.0.113.10:39000".into()),
            traces,
        });
    }
    let executable = tools::resolve("http", root, &config.tools_dir).ok_or_else(|| {
        Error::Run(
            "project-local HTTP report server is not installed; run `asb tool install http` or install it from the setup TUI".into(),
        )
    })?;
    let _owner = lock_project(root)?;
    let path = state_path(root);
    if let Ok(previous) = read_state(&path) {
        stop(&path, &previous)?;
    }
    let generation = crate::tools::runtime::unique_id();
    let publication = root.join(".asb").join(PUBLICATION_DIR).join(&generation);
    materialize(&publication, &report, &traces)?;
    let state = State {
        schema_version: 1,
        generation: Some(generation.clone()),
        port: 0,
        public_host: public_host()?,
        executable: executable.into(),
        publication,
        traces,
        pid: None,
        control: None,
        owner_identity: None,
        server_identity: None,
    };
    let private_state = root
        .join(".asb")
        .join(format!("http-server-{generation}.json"));
    write_state(&private_state, &state)?;
    let asb = std::env::current_exe().map_err(crate::io_error)?;
    let mut wrapper = Command::new(asb)
        .arg("__asb-http-server")
        .arg("--state")
        .arg(&private_state)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::Run(format!("cannot start ASB HTTP report server: {error}")))?;
    let deadline = Instant::now() + STARTUP_BUDGET;
    loop {
        if let Ok(ready) = read_state(&private_state)
            && ready.port != 0
            && ready.pid.is_some()
        {
            write_state(&path, &ready)?;
            if wrapper
                .stdin
                .as_mut()
                .ok_or_else(|| Error::Run("HTTP server startup pipe is unavailable".into()))?
                .write_all(b"A")
                .is_err()
            {
                let _ = fs::remove_file(&path);
                return Err(Error::Run(
                    "HTTP server exited before startup was accepted".into(),
                ));
            }
            drop(wrapper.stdin.take());
            return Ok(ServerInfo {
                base: Some(base_url(&ready.public_host, ready.port)),
                traces: ready.traces,
            });
        }
        if wrapper.try_wait().map_err(crate::io_error)?.is_some() {
            return Err(Error::Run(
                "ASB HTTP report server exited during startup; inspect the project and retry"
                    .into(),
            ));
        }
        if Instant::now() >= deadline {
            return Err(Error::Run(
                "ASB HTTP report server did not become ready; rerun `asb tool http start`".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(crate) fn entry(args: &[std::ffi::OsString]) -> std::process::ExitCode {
    let [flag, path] = args else {
        return std::process::ExitCode::from(2);
    };
    if flag != "--state" {
        return std::process::ExitCode::from(2);
    }
    let lost = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let monitor = std::sync::Arc::clone(&lost);
    if std::thread::Builder::new()
        .name("asb-http-startup-owner".into())
        .spawn(move || {
            let mut byte = [0];
            if std::io::stdin().read_exact(&mut byte).is_err() || byte != *b"A" {
                monitor.store(true, std::sync::atomic::Ordering::Release);
            }
        })
        .is_err()
    {
        return std::process::ExitCode::from(2);
    }
    match read_state(Path::new(path)).and_then(|state| serve(Path::new(path), state, &lost)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::from(2),
    }
}

fn serve(
    state_path: &Path,
    mut state: State,
    owner_lost: &std::sync::atomic::AtomicBool,
) -> Result<(), Error> {
    let control = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(crate::io_error)?;
    control.set_nonblocking(true).map_err(crate::io_error)?;
    let control_address = Control {
        port: control.local_addr().map_err(crate::io_error)?.port(),
        token: crate::tools::runtime::unique_id(),
    };
    for _ in 0..8 {
        let reservation = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(crate::io_error)?;
        let port = reservation.local_addr().map_err(crate::io_error)?.port();
        drop(reservation);
        let mut command = Command::new(&state.executable);
        command
            .args(server_args(port))
            .arg(&state.publication)
            .stdin(Stdio::null())
            .env("FORCE_COLOR", "0")
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        protect_child_from_owner_loss(&mut command);
        let mut child = command.spawn().map_err(|error| {
            Error::Run(format!("cannot start project-local http-server: {error}"))
        })?;
        let (activity_tx, activity_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        if let Some(stream) = child.stdout.take() {
            watch_requests(stream, activity_tx.clone(), Some(ready_tx));
        }
        if let Some(stream) = child.stderr.take() {
            watch_requests(stream, activity_tx.clone(), None);
        }
        drop(activity_tx);
        if !wait_for_ready(&mut child, &ready_rx, START_TIMEOUT, owner_lost) {
            let _ = child.kill();
            let _ = child.wait();
            if owner_lost.load(std::sync::atomic::Ordering::Acquire) {
                let _ = fs::remove_file(state_path);
                let _ = fs::remove_dir_all(&state.publication);
                return Err(Error::Run("HTTP server startup owner exited".into()));
            }
            continue;
        }
        state.port = port;
        state.pid = Some(child.id());
        state.control = Some(control_address.clone());
        #[cfg(target_os = "linux")]
        {
            state.owner_identity = process_identity(std::process::id()).ok();
            state.server_identity = process_identity(child.id()).ok();
        }
        if let Err(error) = write_state(state_path, &state) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(state_path);
            let _ = fs::remove_dir_all(&state.publication);
            return Err(error);
        }
        let mut last_served = Instant::now();
        loop {
            if owner_lost.load(std::sync::atomic::Ordering::Acquire) {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => (),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(crate::io_error(error));
                }
            }
            if let Ok((mut connection, _)) = control.accept() {
                let _ = connection.set_read_timeout(Some(Duration::from_millis(500)));
                let mut line = String::new();
                let valid = BufReader::new(&mut connection)
                    .take(256)
                    .read_line(&mut line)
                    .is_ok()
                    && line.trim_end() == control_address.token;
                if valid {
                    let stopped = child.kill().and_then(|()| child.wait()).is_ok();
                    let response = if stopped {
                        format!("stopped {}\n", control_address.token)
                    } else {
                        "failed\n".into()
                    };
                    let _ = connection.write_all(response.as_bytes());
                    if stopped {
                        break;
                    }
                }
            }
            if activity_rx.recv_timeout(Duration::from_millis(200)).is_ok() {
                last_served = Instant::now();
            }
            if last_served.elapsed() >= IDLE_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
        let _ = fs::remove_file(state_path);
        if let Some(root) = state_path.parent().and_then(Path::parent) {
            let _ = clear_if_owned(root, &state);
        }
        let _ = fs::remove_dir_all(&state.publication);
        return Ok(());
    }
    let _ = fs::remove_file(state_path);
    let _ = fs::remove_dir_all(&state.publication);
    Err(Error::Run(
        "could not reserve a free port for ASB HTTP report server".into(),
    ))
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn protect_child_from_owner_loss(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    let owner = std::process::id();
    // SAFETY: only async-signal-safe prctl/getppid operations occur in the
    // forked child. Check the race where its owner exits before prctl runs.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() as u32 != owner {
                return Err(std::io::Error::other(
                    "HTTP server owner exited during startup",
                ));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn protect_child_from_owner_loss(_: &mut Command) {}

fn server_args(port: u16) -> Vec<String> {
    [
        "-a".to_owned(),
        "0.0.0.0".to_owned(),
        "-p".to_owned(),
        port.to_string(),
        "-c-1".to_owned(),
        "-d".to_owned(),
        "false".to_owned(),
        "-i".to_owned(),
        "false".to_owned(),
    ]
    .to_vec()
}

fn wait_for_ready(
    child: &mut std::process::Child,
    ready: &mpsc::Receiver<()>,
    timeout: Duration,
    owner_lost: &std::sync::atomic::AtomicBool,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if owner_lost.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        match ready.recv_timeout(Duration::from_millis(20)) {
            Ok(()) => return child.try_wait().is_ok_and(|status| status.is_none()),
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    false
}

fn stop(state_path: &Path, state: &State) -> Result<(), Error> {
    if state.pid.is_some()
        && let Err(error) = stop_owned(state.control.as_ref())
        && !definitely_settled(state)
    {
        return Err(error);
    }
    let _ = fs::remove_file(state_path);
    Ok(())
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> std::io::Result<ProcessIdentity> {
    let pid_namespace = current_pid_namespace()?;
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned();
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields = stat
        .rsplit_once(')')
        .ok_or_else(|| std::io::Error::other("malformed process stat"))?
        .1;
    let start_time = fields
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| std::io::Error::other("missing process start time"))?
        .parse()
        .map_err(|_| std::io::Error::other("invalid process start time"))?;
    Ok(ProcessIdentity {
        pid,
        boot_id,
        start_time,
        pid_namespace: Some(pid_namespace),
    })
}

#[cfg(target_os = "linux")]
fn checked_pid_namespace(
    process_id: u32,
    proc_stat: &str,
    namespace: &Path,
) -> std::io::Result<String> {
    let visible = proc_stat
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<u32>().ok());
    if visible != Some(process_id) {
        return Err(std::io::Error::other(
            "proc mount does not match the current PID namespace",
        ));
    }
    namespace
        .to_str()
        .filter(|value| value.starts_with("pid:[") && value.ends_with(']'))
        .map(str::to_owned)
        .ok_or_else(|| std::io::Error::other("cannot identify PID namespace"))
}

#[cfg(target_os = "linux")]
fn current_pid_namespace() -> std::io::Result<String> {
    checked_pid_namespace(
        std::process::id(),
        &fs::read_to_string("/proc/self/stat")?,
        &fs::read_link("/proc/self/ns/pid")?,
    )
}

#[cfg(target_os = "linux")]
fn identity_gone(identity: &ProcessIdentity) -> bool {
    if identity.pid_namespace.as_deref() != current_pid_namespace().ok().as_deref()
        || identity.pid_namespace.is_none()
    {
        return false;
    }
    let boot = match fs::read_to_string("/proc/sys/kernel/random/boot_id") {
        Ok(value) => value,
        Err(_) => return false,
    };
    if boot.trim() != identity.boot_id {
        return true;
    }
    let stat = match fs::read_to_string(format!("/proc/{}/stat", identity.pid)) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
    };
    let Some((_, tail)) = stat.rsplit_once(')') else {
        return false;
    };
    let mut fields = tail.split_whitespace();
    let state = fields.next();
    let start = fields.nth(18).and_then(|value| value.parse::<u64>().ok());
    match start {
        Some(start) if start != identity.start_time => true,
        Some(_) => matches!(state, Some("Z" | "X")),
        None => false,
    }
}

#[cfg(target_os = "linux")]
fn definitely_settled(state: &State) -> bool {
    let (Some(owner), Some(server)) = (&state.owner_identity, &state.server_identity) else {
        return false;
    };
    if state.pid != Some(server.pid)
        || owner.pid <= 1
        || server.pid <= 1
        || owner.pid == server.pid
        || owner.boot_id.is_empty()
        || owner.boot_id != server.boot_id
        || owner.pid_namespace.is_none()
        || owner.pid_namespace != server.pid_namespace
    {
        return false;
    }
    if !identity_gone(owner) {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if identity_gone(server) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(not(target_os = "linux"))]
fn definitely_settled(_: &State) -> bool {
    false
}

fn stop_owned(control: Option<&Control>) -> Result<(), Error> {
    let control = control.ok_or_else(|| Error::Run("HTTP server state has no owned control endpoint; inspect the previous server and remove stale state before retrying".into()))?;
    if control.port == 0
        || control.token.is_empty()
        || control.token.len() > 128
        || !control
            .token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(Error::Run(
            "HTTP server control identity is malformed; state preserved".into(),
        ));
    }
    let address = std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, control.port));
    let mut stream =
        TcpStream::connect_timeout(&address, Duration::from_secs(2)).map_err(|error| {
            Error::Run(format!(
                "cannot contact owned HTTP server control endpoint: {error}; state preserved"
            ))
        })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(crate::io_error)?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(crate::io_error)?;
    writeln!(stream, "{}", control.token).map_err(crate::io_error)?;
    let mut response = String::new();
    BufReader::new(stream)
        .take(256)
        .read_line(&mut response)
        .map_err(crate::io_error)?;
    if response != format!("stopped {}\n", control.token) {
        return Err(Error::Run(
            "HTTP server did not confirm owned shutdown; state preserved".into(),
        ));
    }
    Ok(())
}

fn materialize(destination: &Path, report: &Path, traces: &[PathBuf]) -> Result<(), Error> {
    let _ = fs::remove_dir_all(destination);
    fs::create_dir_all(destination).map_err(crate::io_error)?;
    let source = report
        .parent()
        .ok_or_else(|| Error::Input("report has no parent".into()))?;
    copy_report_assets(source, source, destination, &mut 0usize)?;
    for (index, trace) in traces.iter().enumerate() {
        let target = destination
            .join("traces")
            .join(format!("{index}.{}", trace_extension(trace)));
        fs::create_dir_all(target.parent().expect("trace parent")).map_err(crate::io_error)?;
        fs::copy(trace, target).map_err(crate::io_error)?;
    }
    Ok(())
}

fn copy_report_assets(
    root: &Path,
    directory: &Path,
    destination: &Path,
    count: &mut usize,
) -> Result<(), Error> {
    for entry in fs::read_dir(directory).map_err(crate::io_error)? {
        let entry = entry.map_err(crate::io_error)?;
        let kind = entry.file_type().map_err(crate::io_error)?;
        if kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(|error| {
            Error::Input(format!(
                "report asset is outside its report directory: {error}"
            ))
        })?;
        if kind.is_dir() {
            copy_report_assets(root, &path, destination, count)?;
            continue;
        }
        if !kind.is_file() || !is_report_asset(&path) {
            continue;
        }
        *count += 1;
        if *count > MAX_FILES {
            return Err(Error::Input("too many generated report assets".into()));
        }
        if path.metadata().map_err(crate::io_error)?.len() > MAX_FILE_BYTES {
            return Err(Error::Input(format!(
                "report asset {} exceeds 64 MiB",
                path.display()
            )));
        }
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(crate::io_error)?;
        }
        fs::copy(path, target).map_err(crate::io_error)?;
    }
    Ok(())
}

fn watch_requests(
    stream: impl Read + Send + 'static,
    sender: mpsc::Sender<()>,
    ready: Option<mpsc::Sender<()>>,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            // http-server 14.1.1 emits this line from its listen callback,
            // only after its own listener has successfully bound the port.
            if line.starts_with("Starting up http-server, serving ")
                && let Some(ready) = &ready
            {
                let _ = ready.send(());
            }
            if line.contains("GET ") || line.contains("HEAD ") {
                let _ = sender.send(());
            }
        }
    });
}

fn is_report_asset(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some("html" | "svg" | "json" | "css" | "js")
    )
}
fn state_path(root: &Path) -> PathBuf {
    root.join(".asb").join(STATE_NAME)
}
fn lock_project(root: &Path) -> Result<crate::output_lock::OutputLock, Error> {
    crate::output_lock::OutputLock::acquire_for(
        &root.join(".asb-http-server.lock"),
        "another HTTP report server operation is in progress",
        Duration::from_secs(5),
    )
    .map_err(Error::Run)
}

fn clear_if_owned(root: &Path, state: &State) -> Result<(), Error> {
    let _owner = lock_project(root)?;
    let path = state_path(root);
    if let Ok(current) = read_state(&path)
        && state.generation.is_some()
        && current.generation == state.generation
    {
        fs::remove_file(path).map_err(crate::io_error)?;
    }
    Ok(())
}
fn read_state(path: &Path) -> Result<State, Error> {
    serde_json::from_str(&fs::read_to_string(path).map_err(crate::io_error)?)
        .map_err(|error| Error::Input(format!("invalid HTTP server state: {error}")))
}
fn write_state(path: &Path, state: &State) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(crate::io_error)?;
    }
    fs::write(
        path,
        serde_json::to_vec(state).map_err(|error| Error::Io(error.to_string()))?,
    )
    .map_err(crate::io_error)
}
fn canonical_traces(traces: Vec<PathBuf>) -> Vec<PathBuf> {
    canonical_traces_with(traces, |name, path| {
        crate::collectors::report_artifact_links(name, path)
            .iter()
            .any(|link| link.primary.is_some())
    })
}
fn canonical_traces_with(
    traces: Vec<PathBuf>,
    accepts: impl Fn(&str, &Path) -> bool,
) -> Vec<PathBuf> {
    let mut traces = traces
        .into_iter()
        .filter_map(|trace| {
            let trace = trace.canonicalize().ok()?;
            let name = trace.file_name()?.to_str()?;
            (trace.is_file() && accepts(name, &trace) && trace.metadata().ok()?.len() > 0)
                .then_some(trace)
        })
        .collect::<Vec<_>>();
    traces.sort();
    traces.dedup();
    traces
}
fn trace_extension(trace: &Path) -> &str {
    trace
        .extension()
        .and_then(|value| value.to_str())
        .filter(|extension| {
            !extension.is_empty()
                && extension
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .unwrap_or("artifact")
}
fn latest_report(root: &Path) -> Result<(PathBuf, Vec<PathBuf>), Error> {
    let mut files = Vec::new();
    collect(root, &mut files)?;
    let report = files
        .iter()
        .filter(|path| path.file_name().is_some_and(|name| name == "report.html"))
        .max_by_key(|path| {
            path.metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(UNIX_EPOCH)
        })
        .cloned()
        .ok_or_else(|| {
            Error::Input(
                "no generated report.html found; run `asb compare CONFIG.toml` first".into(),
            )
        })?;
    let traces = files
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    crate::collectors::report_artifact_links(name, path)
                        .iter()
                        .any(|link| link.primary.is_some())
                })
        })
        .collect();
    Ok((report, traces))
}
fn collect(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), Error> {
    if files.len() > MAX_FILES {
        return Err(Error::Input(
            "too many project files while locating report artifacts".into(),
        ));
    }
    for entry in fs::read_dir(root).map_err(crate::io_error)? {
        let entry = entry.map_err(crate::io_error)?;
        let kind = entry.file_type().map_err(crate::io_error)?;
        if kind.is_symlink()
            || entry.file_name() == ".asb-tools"
            || (root.file_name().is_some_and(|name| name == ".asb")
                && entry.file_name() == PUBLICATION_DIR)
        {
            continue;
        }
        if kind.is_dir() {
            collect(&entry.path(), files)?;
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}
fn public_host() -> Result<String, Error> {
    if let Ok(host) = std::env::var("ASB_HTTP_PUBLIC_HOST")
        && valid_host(&host)
    {
        return Ok(host);
    }
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(crate::io_error)?;
    socket.connect("1.1.1.1:80").map_err(crate::io_error)?;
    let IpAddr::V4(address) = socket.local_addr().map_err(crate::io_error)?.ip() else {
        return Err(Error::Input(
            "set ASB_HTTP_PUBLIC_HOST to this machine's public IP address".into(),
        ));
    };
    if address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
    {
        return Err(Error::Input("cannot determine a public ASB HTTP address; set ASB_HTTP_PUBLIC_HOST to this machine's public IP address".into()));
    }
    Ok(address.to_string())
}
fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':' | b'[' | b']')
        })
}
fn base_url(host: &str, port: u16) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.into()
    };
    format!("http://{host}:{port}")
}

fn local_file_url(path: &Path) -> String {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path = path.to_string_lossy().replace('\\', "/");
    let path = percent_encode_path(&path);
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

fn percent_encode_path(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'-' | b'_' | b'~' => {
                vec![byte as char]
            }
            other => format!("%{other:02X}").chars().collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_process_identity_requires_an_exact_live_instance() {
        let Ok(current) = process_identity(std::process::id()) else {
            let unknown = ProcessIdentity {
                pid: std::process::id(),
                boot_id: String::new(),
                start_time: 0,
                pid_namespace: None,
            };
            assert!(!identity_gone(&unknown));
            return;
        };
        assert!(!identity_gone(&current));
        let mut reused = current.clone();
        reused.start_time = reused.start_time.saturating_add(1);
        assert!(identity_gone(&reused));
        let mut previous_boot = current;
        previous_boot.boot_id = "previous-boot".into();
        assert!(identity_gone(&previous_boot));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pid_namespace_and_proc_mount_must_match() {
        assert!(checked_pid_namespace(5, "4 (fixture) S", Path::new("pid:[42]")).is_err());
        assert!(checked_pid_namespace(5, "5 (fixture) S", Path::new("pid:[42]")).is_ok());
        let Ok(mut identity) = process_identity(std::process::id()) else {
            return;
        };
        identity.start_time += 1;
        identity.pid_namespace = Some("pid:[999999999999]".into());
        assert!(!identity_gone(&identity));
        identity.pid_namespace = None;
        assert!(!identity_gone(&identity));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn nested_pid_namespace_cannot_retire_host_process_identity() {
        const FIXTURE: &str = "ASB_HTTP_HOST_PID_IDENTITY";
        const MISMATCH: &str = "ASB_HTTP_MISMATCHED_PROC";
        if std::env::var_os(MISMATCH).is_some() {
            assert!(
                current_pid_namespace().is_err(),
                "a host proc mount must not authorize nested PID lookups"
            );
            return;
        }
        if let Ok(encoded) = std::env::var(FIXTURE) {
            let identity: ProcessIdentity = serde_json::from_str(&encoded).unwrap();
            assert_ne!(
                identity.pid_namespace.as_deref(),
                Some(current_pid_namespace().unwrap().as_str())
            );
            assert!(
                !identity_gone(&identity),
                "an inaccessible host PID is not proof of exit"
            );
            return;
        }
        let Ok(identity) = process_identity(std::process::id()) else {
            return;
        };
        let output = match Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--pid",
                "--fork",
                "--mount-proc",
            ])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "http_server::tests::nested_pid_namespace_cannot_retire_host_process_identity",
                "--nocapture",
            ])
            .env(FIXTURE, serde_json::to_string(&identity).unwrap())
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("cannot start namespace fixture: {error}"),
        };
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success()
            && (stderr.contains("Operation not permitted") || stderr.contains("Permission denied"))
        {
            eprintln!("nested PID namespace fixture unavailable: {stderr}");
            return;
        }
        assert!(
            output.status.success(),
            "namespace fixture failed: {stderr} {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let mismatch = Command::new("unshare")
            .args(["--user", "--map-root-user", "--pid", "--fork"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "http_server::tests::nested_pid_namespace_cannot_retire_host_process_identity",
                "--nocapture",
            ])
            .env(MISMATCH, "1")
            .output()
            .unwrap();
        assert!(
            mismatch.status.success(),
            "mismatched proc fixture failed: {} {}",
            String::from_utf8_lossy(&mismatch.stderr),
            String::from_utf8_lossy(&mismatch.stdout)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stopped_wrapper_and_server_are_not_settled() {
        if process_identity(std::process::id()).is_err() {
            return;
        }
        struct Children(Vec<std::process::Child>);
        impl Drop for Children {
            fn drop(&mut self) {
                for child in &mut self.0 {
                    let _ = Command::new("kill")
                        .args(["-CONT", &child.id().to_string()])
                        .status();
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let mut children = Children(vec![
            Command::new("sleep").arg("30").spawn().unwrap(),
            Command::new("sleep").arg("30").spawn().unwrap(),
        ]);
        let owner = process_identity(children.0[0].id()).unwrap();
        let server = process_identity(children.0[1].id()).unwrap();
        for child in &mut children.0 {
            assert!(
                Command::new("kill")
                    .args(["-STOP", &child.id().to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let status = fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap();
                if status.lines().any(|line| {
                    line.starts_with("State:") && line.split_whitespace().nth(1) == Some("T")
                }) {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(!identity_gone(&owner));
        assert!(!identity_gone(&server));
        let state = State {
            schema_version: 1,
            generation: None,
            port: 0,
            public_host: String::new(),
            executable: PathBuf::new(),
            publication: PathBuf::new(),
            traces: vec![],
            pid: Some(server.pid),
            control: None,
            owner_identity: Some(owner),
            server_identity: Some(server),
        };
        assert!(!definitely_settled(&state));
    }

    #[test]
    fn foreign_listener_cannot_make_an_unrelated_server_child_ready() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let (_sender, receiver) = mpsc::channel();
        let ready = wait_for_ready(
            &mut child,
            &receiver,
            Duration::from_millis(100),
            &std::sync::atomic::AtomicBool::new(false),
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            !ready,
            "a foreign listener cannot prove this child bound the port"
        );
        drop(listener);
    }

    #[test]
    fn owned_child_listen_callback_is_required_for_readiness() {
        let mut child = Command::new("sh")
            .args([
                "-c",
                "printf 'Starting up http-server, serving fixture\\n'; exec sleep 30",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let (activity, _events) = mpsc::channel();
        let (ready, receiver) = mpsc::channel();
        watch_requests(child.stdout.take().unwrap(), activity, Some(ready));
        let admitted = wait_for_ready(
            &mut child,
            &receiver,
            Duration::from_secs(2),
            &std::sync::atomic::AtomicBool::new(false),
        );
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(admitted);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_server_pid_never_signals_an_unrelated_process() {
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        fs::create_dir_all(root.join(".asb")).unwrap();
        let mut sentinel = Command::new("sleep").arg("30").spawn().unwrap();
        let mut state = State {
            schema_version: 1,
            generation: Some("stale".into()),
            port: 1,
            public_host: "localhost".into(),
            executable: "fixture".into(),
            publication: root.join("publication"),
            traces: vec![],
            pid: Some(sentinel.id()),
            control: None,
            owner_identity: process_identity(std::process::id()).ok(),
            server_identity: process_identity(sentinel.id()).ok(),
        };
        let path = state_path(&root);
        write_state(&path, &state).unwrap();
        let result = stop(&path, &state);
        if let Some(owner) = state.owner_identity.as_mut() {
            owner.start_time += 1;
        }
        let dead_owner_live_server = stop(&path, &state);
        std::thread::sleep(Duration::from_millis(50));
        let still_running = sentinel.try_wait().unwrap().is_none();
        let state_preserved = path.exists();
        let _ = sentinel.kill();
        let _ = sentinel.wait();
        fs::remove_dir_all(root).unwrap();
        assert!(result.is_err());
        assert!(
            dead_owner_live_server.is_err(),
            "a surviving server must retain state"
        );
        assert!(
            still_running,
            "stale PID must not signal an unrelated process"
        );
        assert!(
            state_preserved,
            "unverified state must remain available for inspection"
        );
    }

    #[test]
    fn old_server_cleanup_preserves_a_new_server_state_and_report() {
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        fs::create_dir_all(root.join(".asb")).unwrap();
        let make = |generation: &str| State {
            schema_version: 1,
            generation: Some(generation.into()),
            port: 1,
            public_host: "localhost".into(),
            executable: "fixture".into(),
            publication: root.join(".asb/http-publication").join(generation),
            traces: vec![],
            pid: None,
            control: None,
            owner_identity: None,
            server_identity: None,
        };
        let old = make("old");
        let new = make("new");
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("report.html"), "old report").unwrap();
        materialize(&old.publication, &source.join("report.html"), &[]).unwrap();
        fs::write(source.join("report.html"), "new report").unwrap();
        materialize(&new.publication, &source.join("report.html"), &[]).unwrap();
        write_state(&state_path(&root), &new).unwrap();
        clear_if_owned(&root, &old).unwrap();
        assert_eq!(
            read_state(&state_path(&root))
                .unwrap()
                .generation
                .as_deref(),
            Some("new")
        );
        assert_eq!(
            fs::read_to_string(new.publication.join("report.html")).unwrap(),
            "new report"
        );
        assert_eq!(
            fs::read_to_string(old.publication.join("report.html")).unwrap(),
            "old report"
        );
        clear_if_owned(&root, &new).unwrap();
        assert!(!state_path(&root).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_does_not_treat_published_copies_as_reports() {
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        let report = root.join("results/report.html");
        let published = root.join(".asb/http-publication/old/report.html");
        fs::create_dir_all(report.parent().unwrap()).unwrap();
        fs::create_dir_all(published.parent().unwrap()).unwrap();
        fs::write(&report, "original").unwrap();
        fs::write(&published, "copy").unwrap();
        let mut found = Vec::new();
        collect(&root, &mut found).unwrap();
        assert!(found.contains(&report));
        assert!(!found.contains(&published));
        assert_eq!(latest_report(&root).unwrap().0, report);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn synthetic_collector_artifact_is_accepted_and_keeps_its_extension() {
        let root = std::env::temp_dir().join(format!(
            "asb-http-other-{}",
            crate::tools::runtime::unique_id()
        ));
        fs::create_dir_all(&root).unwrap();
        let trace = root.join("other.trace");
        fs::write(&trace, b"trace").unwrap();
        let traces = canonical_traces_with(vec![trace.clone()], |name, _| name == "other.trace");
        assert_eq!(traces, vec![trace.canonicalize().unwrap()]);
        let server = ServerInfo {
            base: Some("http://localhost:1234".into()),
            traces,
        };
        assert_eq!(
            server.trace_url(&trace).as_deref(),
            Some("http://localhost:1234/traces/0.trace")
        );
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn report_assets_and_traces_are_materialized_without_other_files() {
        let root = std::env::temp_dir().join(format!(
            "asb-http-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("report.html"), "report").unwrap();
        fs::write(source.join("graph.svg"), "svg").unwrap();
        fs::write(source.join("private.txt"), "private").unwrap();
        let trace = root.join("trace.perfetto-trace");
        fs::write(&trace, "trace").unwrap();
        let output = root.join("output");
        materialize(&output, &source.join("report.html"), &[trace]).unwrap();
        assert!(output.join("report.html").is_file());
        assert!(output.join("graph.svg").is_file());
        assert!(!output.join("private.txt").exists());
        assert!(output.join("traces/0.perfetto-trace").is_file());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn server_disables_directory_listing_without_using_version_flag() {
        let args = server_args(42);
        assert_eq!(
            args,
            [
                "-a", "0.0.0.0", "-p", "42", "-c-1", "-d", "false", "-i", "false"
            ]
        );
        assert!(!args.iter().any(|argument| argument == "-v"));
    }

    #[test]
    fn report_urls_use_files_until_the_explicit_server_is_available() {
        let report = Path::new("/tmp/asb-report.html");
        let local = ServerInfo {
            base: None,
            traces: Vec::new(),
        };
        assert_eq!(local.report_url(report), "file:///tmp/asb-report.html");
        let served = ServerInfo {
            base: Some("http://203.0.113.10:39000".into()),
            traces: Vec::new(),
        };
        assert_eq!(
            served.report_url(report),
            "http://203.0.113.10:39000/report.html"
        );
    }
}
