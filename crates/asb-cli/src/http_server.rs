//! Lifecycle wrapper around the project-local `http-server` tool.
//!
//! ASB materializes an allowlisted publication directory, then delegates HTTP
//! protocol handling to the established `http-server` package. The wrapper
//! owns only port selection, per-project state, idle expiry and process
//! termination; it never exposes the project directory itself.

use crate::{Error, install, project, tools};
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
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 10_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct State {
    schema_version: u8,
    port: u16,
    public_host: String,
    executable: PathBuf,
    publication: PathBuf,
    traces: Vec<PathBuf>,
    pid: Option<u32>,
}

/// URLs served by this project's current report server.
pub(crate) struct ServerInfo {
    base: String,
    traces: Vec<PathBuf>,
}
impl ServerInfo {
    pub(crate) fn report_url(&self) -> String {
        format!("{}/report.html", self.base)
    }
    pub(crate) fn trace_url(&self, trace: &Path) -> Option<String> {
        let trace = trace.canonicalize().ok()?;
        self.traces
            .iter()
            .position(|item| item == &trace)
            .map(|index| format!("{}/traces/{index}.perfetto-trace", self.base))
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
    start(&root, &config, report, traces, output)
}

pub(crate) fn start_manual(output: &mut dyn Write) -> Result<(), Error> {
    let (root, config) = project::load(Path::new("."))?;
    let (report, traces) = latest_report(&root)?;
    let server = start(&root, &config, &report, traces, output)?;
    writeln!(
        output,
        "HTTP report server: {}\nServes only the generated report and its Perfetto traces; stops one hour after the last request.",
        server.report_url()
    )
    .map_err(crate::io_error)
}

pub(crate) fn stop_manual(output: &mut dyn Write) -> Result<(), Error> {
    let (root, _) = project::load(Path::new("."))?;
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
    output: &mut dyn Write,
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
            base: "http://203.0.113.10:39000".into(),
            traces,
        });
    }
    if tools::resolve("http", root, &config.tools_dir).is_none() {
        writeln!(output, "Installing project-local HTTP report server…")
            .map_err(crate::io_error)?;
        install::ensure_project_tool("http", root, config, output)?;
    }
    let executable = tools::resolve("http", root, &config.tools_dir).ok_or_else(|| {
        Error::Run(
            "project-local HTTP report server installation did not publish an executable".into(),
        )
    })?;
    let path = state_path(root);
    if let Ok(previous) = read_state(&path) {
        let _ = stop(&path, &previous);
    }
    let publication = root.join(".asb").join(PUBLICATION_DIR);
    materialize(&publication, &report, &traces)?;
    let state = State {
        schema_version: 1,
        port: 0,
        public_host: public_host()?,
        executable: executable.into(),
        publication,
        traces,
        pid: None,
    };
    write_state(&path, &state)?;
    let asb = std::env::current_exe().map_err(crate::io_error)?;
    Command::new(asb)
        .arg("__asb-http-server")
        .arg("--state")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error::Run(format!("cannot start ASB HTTP report server: {error}")))?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(ready) = read_state(&path)
            && ready.port != 0
            && ready.pid.is_some()
        {
            return Ok(ServerInfo {
                base: base_url(&ready.public_host, ready.port),
                traces: ready.traces,
            });
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
    match read_state(Path::new(path)).and_then(|state| serve(Path::new(path), state)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::from(2),
    }
}

fn serve(state_path: &Path, mut state: State) -> Result<(), Error> {
    for _ in 0..8 {
        let reservation = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(crate::io_error)?;
        let port = reservation.local_addr().map_err(crate::io_error)?.port();
        drop(reservation);
        let mut child = Command::new(&state.executable)
            .args(server_args(port))
            .arg(&state.publication)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                Error::Run(format!("cannot start project-local http-server: {error}"))
            })?;
        if !wait_for_port(port, &mut child) {
            let _ = child.kill();
            let _ = child.wait();
            continue;
        }
        state.port = port;
        state.pid = Some(child.id());
        write_state(state_path, &state)?;
        let (activity_tx, activity_rx) = mpsc::channel();
        if let Some(stream) = child.stdout.take() {
            watch_requests(stream, activity_tx.clone());
        }
        if let Some(stream) = child.stderr.take() {
            watch_requests(stream, activity_tx.clone());
        }
        drop(activity_tx);
        let mut last_served = Instant::now();
        loop {
            if child.try_wait().map_err(crate::io_error)?.is_some() {
                break;
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
        return Ok(());
    }
    Err(Error::Run(
        "could not reserve a free port for ASB HTTP report server".into(),
    ))
}

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

fn wait_for_port(port: u16, child: &mut std::process::Child) -> bool {
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return true;
        }
        if child.try_wait().ok().flatten().is_some() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn stop(state_path: &Path, state: &State) -> Result<(), Error> {
    if let Some(pid) = state.pid {
        #[cfg(windows)]
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
        #[cfg(not(windows))]
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
    let _ = fs::remove_file(state_path);
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
            .join(format!("{index}.perfetto-trace"));
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

fn watch_requests(stream: impl Read + Send + 'static, sender: mpsc::Sender<()>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
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
    let mut traces = traces
        .into_iter()
        .filter_map(|trace| {
            let trace = trace.canonicalize().ok()?;
            (trace.is_file()
                && trace.extension().and_then(|value| value.to_str()) == Some("perfetto-trace")
                && trace.metadata().ok()?.len() > 0)
                .then_some(trace)
        })
        .collect::<Vec<_>>();
    traces.sort();
    traces.dedup();
    traces
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
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("perfetto-trace"))
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
        if kind.is_symlink() || entry.file_name() == ".asb-tools" {
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
