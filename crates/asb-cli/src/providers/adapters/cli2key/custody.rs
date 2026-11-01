//! Direct-child custody and bounded, continuously drained startup output.
use crate::process_cleanup::OwnedReaper;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(10);
const MAX_LINE: usize = 4096;
const MAX_BYTES: usize = 64 * 1024;
const MAX_LINES: usize = 64;

struct Reader {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    proxy: bool,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // The nonblocking worker retains its stream until it observes stop.
        // Never join a potentially unscheduled thread from a destructor.
        if self.worker.as_ref().is_some_and(JoinHandle::is_finished) {
            let _ = self.worker.take().unwrap().join();
        }
    }
}

/// The same owner survives startup and the ready provider session.
pub(super) struct Local {
    children: Vec<Child>,
    readers: Vec<Reader>,
    reaper: OwnedReaper,
    proxy: Option<super::proxy::Watch>,
    reservation: Option<super::proxy::Reservation>,
}

impl Local {
    pub(super) fn new() -> Result<Self, String> {
        Ok(Self {
            children: Vec::with_capacity(2),
            readers: Vec::with_capacity(2),
            reaper: OwnedReaper::new("cli2key")?,
            proxy: None,
            reservation: None,
        })
    }

    pub(super) fn spawn_bridge(&mut self, command: &mut Command) -> Result<String, String> {
        self.spawn_output(command, None)
    }

    pub(super) fn spawn_proxy(
        &mut self,
        command: Command,
    ) -> Result<std::net::SocketAddrV4, String> {
        let reservation = super::proxy::Reservation::new()
            .map_err(|_| "cannot reserve cli2key compatibility listener".to_owned())?;
        self.spawn_reserved_proxy(command, reservation)
    }

    fn spawn_reserved_proxy(
        &mut self,
        mut command: Command,
        reservation: super::proxy::Reservation,
    ) -> Result<std::net::SocketAddrV4, String> {
        let watch = super::proxy::Watch::new(reservation.address());
        reservation.configure(&mut command);
        self.reservation = Some(reservation);
        self.proxy = Some(watch.clone());
        let prior_children = self.children.len();
        let result = self.spawn_output(&mut command, Some(watch.clone()));
        // Command retains its pre_exec closure, hence an Arc to the parent
        // listener. Drop it explicitly on BOTH result paths, before finish.
        drop(command);
        if result.is_err() && self.children.len() == prior_children {
            // No child adopted the listener: a reader/allocation/spawn failure
            // is not an unconfirmed producer or incomplete proxy protocol.
            self.proxy.take();
            self.reservation.take();
        }
        result?;
        watch
            .check()?
            .ok_or_else(|| "cli2key compatibility proxy has no ready address".into())
    }

    fn spawn_output(
        &mut self,
        command: &mut Command,
        proxy: Option<super::proxy::Watch>,
    ) -> Result<String, String> {
        let key = proxy.is_none();
        let label = if key {
            "cli2key bridge"
        } else {
            "cli2key compatibility proxy"
        };
        // Allocate the transport and fallible reader BEFORE creating a child.
        let (mut input, output) = UnixStream::pair()
            .map_err(|error| format!("cannot establish cli2key output transport: {error}"))?;
        input
            .set_nonblocking(true)
            .map_err(|error| format!("cannot bound cli2key output transport: {error}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let (sender, receiver) = mpsc::sync_channel(1);
        #[cfg(all(test, target_os = "linux"))]
        tests::reader_creation()?;
        #[cfg(all(test, target_os = "linux"))]
        let receipt = tests::reader_receipt();
        let worker = std::thread::Builder::new()
            .name("asb-cli2key-output".into())
            .spawn(move || {
                match proxy {
                    Some(watch) => drain_proxy(&mut input, &stopped, &sender, watch),
                    None => drain_output(&mut input, &stopped, &sender, true),
                }
                #[cfg(all(test, target_os = "linux"))]
                receipt.store(true, Ordering::Release);
            })
            .map_err(|error| format!("cannot establish cli2key output reader: {error}"))?;
        self.readers.push(Reader {
            stop,
            worker: Some(worker),
            proxy: !key,
        });
        command.stdout(Stdio::from(OwnedFd::from(output)));
        match command.spawn() {
            Ok(child) => self.children.push(child), // Immediate custody.
            Err(error) => {
                command.stdout(Stdio::null());
                return Err(format!(
                    "could not start {label}: {error}. {}",
                    if key {
                        "Check directory access and reinstall with `asb tool install cli2key`."
                    } else {
                        "Check Node.js with `node --version` and verify executable permissions."
                    }
                ));
            }
        }
        // Command retains its configured Stdio across spawn. Close that parent
        // writer so child-side stdout closure really reaches the reader as EOF.
        command.stdout(Stdio::null());
        #[cfg(all(test, target_os = "linux"))]
        tests::after_spawn(self.children.last().unwrap().id())?;
        let deadline = Instant::now() + super::STARTUP_TIMEOUT;
        loop {
            if !key && let Some(watch) = &self.proxy {
                watch.check()?;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("{label} did not become ready within 5 seconds"));
            }
            match receiver.recv_timeout(remaining.min(POLL)) {
                Ok(result) => {
                    let value = result?;
                    if !key && let Some(watch) = &self.proxy {
                        watch.check()?;
                    }
                    if let Some(status) = self
                        .children
                        .last_mut()
                        .unwrap()
                        .try_wait()
                        .map_err(|error| format!("cannot inspect cli2key readiness: {error}"))?
                    {
                        return Err(format!("{label} exited at readiness with {status}"));
                    }
                    return Ok(value);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(format!(
                        "{label} closed its startup output; {label} did not become ready"
                    ));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if let Some(status) = self
                .children
                .last_mut()
                .unwrap()
                .try_wait()
                .map_err(|error| format!("cannot inspect cli2key startup: {error}"))?
            {
                return Err(format!(
                    "{label} closed its startup output with {status}; {label} did not become ready"
                ));
            }
        }
    }

    #[cfg(test)]
    pub(super) fn failure(mut self, reason: String) -> String {
        match self.cleanup() {
            Ok(()) => reason,
            Err(cleanup) => format!("{reason}; {cleanup}"),
        }
    }

    /// Observe every required child at the final session admission boundary.
    /// This does not promise that a process cannot exit immediately afterward.
    pub(super) fn confirm_ready(&mut self) -> Result<(), String> {
        if let Some(watch) = &self.proxy {
            watch.check()?;
        }
        for (index, child) in self.children.iter_mut().enumerate() {
            let label = if index == 0 {
                "bridge"
            } else {
                "compatibility proxy"
            };
            if let Some(status) = child.try_wait().map_err(|error| {
                format!("cannot inspect cli2key {label} before session admission: {error}")
            })? {
                return Err(format!(
                    "cli2key {label} exited before session admission with {status}"
                ));
            }
        }
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<(), String> {
        self.cleanup()
    }

    fn cleanup(&mut self) -> Result<(), String> {
        let timeout = Duration::from_secs(5);
        #[cfg(all(test, target_os = "linux"))]
        let timeout = tests::cleanup_timeout().unwrap_or(timeout);
        let deadline = Instant::now() + timeout;
        let mut reasons = Vec::new();
        for mut child in self.children.drain(..).rev() {
            // The embedded compatibility hop owns a stdin lifetime pipe.
            // EOF lets it close its own outbound transports before escalation.
            if child.stdin.take().is_some() {
                let grace = (Instant::now() + Duration::from_secs(2)).min(deadline);
                match wait_until(&mut child, grace) {
                    Ok(Some(status)) => {
                        if !status.success() { reasons.push("compatibility proxy exited without confirming cooperative cleanup".into()); }
                        continue;
                    },
                    Ok(None) => reasons.push("compatibility proxy did not confirm cooperative shutdown before its grace deadline".into()),
                    Err(error) => {
                        reasons.push(format!("cannot confirm cooperative child exit: {error}"));
                        self.reaper.adopt(child);
                        continue;
                    }
                }
            }
            let _ = child.kill();
            match wait_until(&mut child, deadline) {
                Ok(Some(_)) => {}
                result => {
                    reasons.push(match result {
                        Err(error) => format!("cannot confirm child exit: {error}"),
                        _ => "child did not exit before the shared cleanup deadline".into(),
                    });
                    self.reaper.adopt(child);
                }
            }
        }
        for reader in &self.readers {
            if !reader.proxy {
                reader.stop.store(true, Ordering::Release);
            }
        }
        for reader in &mut self.readers {
            while reader
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
                && Instant::now() < deadline
            {
                std::thread::sleep(POLL);
            }
            if reader.worker.as_ref().is_some_and(JoinHandle::is_finished) {
                let _ = reader.worker.take().unwrap().join();
            } else {
                reader.stop.store(true, Ordering::Release);
                reasons
                    .push("output reader did not finish before the shared cleanup deadline".into());
            }
        }
        self.readers.clear();
        if let Some(watch) = self.proxy.take() {
            if let Err(reason) = watch.check() {
                reasons.push(reason);
            }
            if !watch.ended() {
                reasons.push("compatibility ready observer settlement was not confirmed".into());
            }
        }
        // Child owns its inherited reference independently if handed to the
        // reaper. Closing ours is not a claim that an uncertain child stopped.
        self.reservation.take();
        if !reasons.is_empty() {
            Err(format!(
                "cli2key cleanup was not confirmed: {}; an owned reaper retains any unconfirmed direct children and stopped readers retain their transports",
                reasons.join("; ")
            ))
        } else {
            Ok(())
        }
    }
}

struct Readiness {
    line: Vec<u8>,
    bytes: usize,
    lines: usize,
}

impl Readiness {
    fn new() -> Self {
        Self {
            line: Vec::with_capacity(MAX_LINE),
            bytes: 0,
            lines: 0,
        }
    }

    fn accept(&mut self, chunk: &[u8], key: bool) -> Option<Result<String, String>> {
        for &byte in chunk {
            self.bytes += 1;
            if self.bytes > MAX_BYTES || (byte != b'\n' && self.line.len() >= MAX_LINE) {
                return Some(Err(
                    "cli2key startup output exceeded its byte or line limit".into(),
                ));
            }
            if byte != b'\n' {
                self.line.push(byte);
                continue;
            }
            self.lines += 1;
            let text = std::str::from_utf8(&self.line)
                .ok()
                .map(|text| text.trim_end_matches('\r'));
            let ready = if key {
                text.and_then(|text| text.strip_prefix("Codex Bridge API key: "))
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            } else {
                text.filter(|text| *text == "ASB cli2key proxy ready")
                    .map(|_| String::new())
            };
            self.line.clear();
            if let Some(value) = ready {
                return Some(Ok(value));
            }
            if self.lines >= MAX_LINES {
                return Some(Err("cli2key startup output exceeded its message limit; bridge did not provide an API key or proxy readiness".into()));
            }
        }
        None
    }
}

fn drain_output(
    input: &mut UnixStream,
    stopped: &AtomicBool,
    sender: &mpsc::SyncSender<Result<String, String>>,
    key: bool,
) {
    let mut startup = Some(Readiness::new());
    let mut chunk = [0; 2048];
    while !stopped.load(Ordering::Acquire) {
        match input.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                if let Some(parser) = &mut startup
                    && let Some(result) = parser.accept(&chunk[..count], key)
                {
                    let _ = sender.try_send(result);
                    // Drain/discard subsequent bytes without parsing or retaining them.
                    startup = None;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(POLL)
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn drain_proxy(
    input: &mut UnixStream,
    stopped: &AtomicBool,
    sender: &mpsc::SyncSender<Result<String, String>>,
    watch: super::proxy::Watch,
) {
    let mut parser = super::proxy::Parser::new(watch.clone());
    let mut notified = false;
    let mut chunk = [0; 2048];
    while !stopped.load(Ordering::Acquire) {
        match input.read(&mut chunk) {
            Ok(0) => {
                parser.end();
                break;
            }
            Ok(count) => {
                // Parse the WHOLE turn, not just the first ready line. Sticky
                // shared faults cannot be lost behind a queued success event.
                parser.accept(&chunk[..count]);
                match watch.check() {
                    Err(error) => {
                        let _ = sender.try_send(Err(error));
                    }
                    Ok(Some(_)) if !notified => {
                        let _ = sender.try_send(Ok(String::new()));
                        notified = true;
                    }
                    _ => {}
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(POLL)
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                parser.read_failed();
                break;
            }
        }
    }
}

fn poll(child: &mut Child) -> std::io::Result<Option<std::process::ExitStatus>> {
    #[cfg(all(test, target_os = "linux"))]
    if let Some(result) = tests::poll_override() {
        return result;
    }
    child.try_wait()
}

fn wait_until(
    child: &mut Child,
    deadline: Instant,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    loop {
        match poll(child)? {
            Some(status) => return Ok(Some(status)),
            None if Instant::now() < deadline => std::thread::sleep(POLL),
            None => return Ok(None),
        }
    }
}

impl Drop for Local {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "../../../../tests/unit/cli2key_custody.rs"]
mod tests;
