//! Owned bounded child pipes. No output reader thread can outlive settlement.
//!
//! Execution, post-exit draining and child cleanup have separate absolute
//! deadlines. A borrowed `Write` remains caller-controlled: an arbitrary
//! blocking writer cannot be made cancellable by this pipe pump.
#![allow(unsafe_code)]

use crate::process_cleanup::OwnedReaper;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Output};
use std::time::{Duration, Instant};

#[cfg(unix)]
pub(crate) mod transport;

pub(super) const CAPTURE_LIMIT: usize = 8 * 1024 * 1024;
const READ_BATCH: usize = 64 * 1024;
const TICK: Duration = Duration::from_millis(5);
const DRAIN: Duration = Duration::from_secs(1);
const CLEANUP: Duration = Duration::from_secs(6);

pub(super) struct DirectChild {
    child: Option<Child>,
    keeper: OwnedReaper,
    observed: Option<ExitStatus>,
}

impl DirectChild {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        if cfg!(not(any(unix, windows))) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "nonblocking child pipes require a platform implementation",
            ));
        }
        let keeper = OwnedReaper::new("agent-direct-custody").map_err(io::Error::other)?;
        let child = command.spawn()?;
        Ok(Self {
            child: Some(child),
            keeper,
            observed: None,
        })
    }
    fn id(&self) -> u32 {
        self.child.as_ref().expect("owned child").id()
    }
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.observed {
            return Ok(Some(status));
        }
        let status = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("direct child retained in cleanup custody"))?
            .try_wait()?;
        self.observed = status;
        Ok(status)
    }
    fn handoff(&mut self) {
        if let Some(child) = self.child.take() {
            self.keeper.adopt(child);
        }
    }
    fn stop(&mut self) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + CLEANUP;
        if let Some(status) = self.observed {
            return Ok(status);
        }
        let signal = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("direct child retained in cleanup custody"))?
            .kill();
        loop {
            #[cfg(test)]
            let poll = test_support::stop_poll().unwrap_or_else(|| self.try_wait());
            #[cfg(not(test))]
            let poll = self.try_wait();
            match poll {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => (),
                Err(error) => {
                    self.handoff();
                    return Err(io::Error::other(format!(
                        "{error}; direct child retained in cleanup custody"
                    )));
                }
            }
            if Instant::now() >= deadline {
                self.handoff();
                return Err(io::Error::other(match signal {
                    Ok(()) => {
                        "direct child cleanup unconfirmed; retained in cleanup custody".into()
                    }
                    Err(error) => {
                        format!("cannot stop direct child: {error}; retained in cleanup custody")
                    }
                }));
            }
            std::thread::sleep(TICK);
        }
    }
}
impl Drop for DirectChild {
    fn drop(&mut self) {
        if self.child.is_some() && self.observed.is_none() {
            // Normal paths compose this error with the primary failure. Drop
            // only retains custody: even printing to stderr can block forever.
            let _ = self.stop();
        }
    }
}

pub(super) enum OwnedChild {
    Direct(DirectChild),
    #[cfg(target_os = "linux")]
    Gated(crate::process_gate::ManagedGate),
}
impl OwnedChild {
    fn observation(&self) -> crate::process_termination::Observation {
        match self {
            Self::Direct(child) => child
                .observed
                .map(crate::process_termination::Observation::observed)
                .unwrap_or_default(),
            #[cfg(target_os = "linux")]
            Self::Gated(child) => child.principal_observation(),
        }
    }
    #[cfg(unix)]
    pub fn wait_stopped(&mut self) -> io::Result<()> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Gated(gate) => gate.wait_stopped(),
            Self::Direct(_) => Err(io::Error::other(
                "shared launch refuses a direct child without gate custody",
            )),
        }
    }
    pub fn spawn_direct(command: &mut Command) -> io::Result<Self> {
        DirectChild::spawn(command).map(Self::Direct)
    }
    pub fn stop(&mut self) -> io::Result<ExitStatus> {
        match self {
            Self::Direct(child) => child.stop(),
            #[cfg(target_os = "linux")]
            Self::Gated(child) => child.stop(),
        }
    }
    pub fn failure(&mut self, primary: impl std::fmt::Display) -> String {
        match self.stop() {
            Ok(_) => primary.to_string(),
            Err(cleanup) => format!("{primary}; owned child cleanup also failed: {cleanup}"),
        }
    }
    pub fn id(&self) -> u32 {
        match self {
            Self::Direct(child) => child.id(),
            #[cfg(target_os = "linux")]
            Self::Gated(child) => child.id(),
        }
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            Self::Direct(child) => child.try_wait(),
            #[cfg(target_os = "linux")]
            Self::Gated(child) => child.try_wait(),
        }
    }
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        match self {
            Self::Direct(child) => child.child.as_mut()?.stdout.take(),
            #[cfg(target_os = "linux")]
            Self::Gated(child) => child.take_stdout(),
        }
    }
    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        match self {
            Self::Direct(child) => child.child.as_mut()?.stdin.take(),
            #[cfg(target_os = "linux")]
            Self::Gated(child) => child.take_stdin(),
        }
    }
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        match self {
            Self::Direct(child) => child.child.as_mut()?.stderr.take(),
            #[cfg(target_os = "linux")]
            Self::Gated(child) => child.take_stderr(),
        }
    }
    #[cfg(unix)]
    pub fn resume(&mut self) -> io::Result<()> {
        match self {
            Self::Direct(child) => {
                crate::process_gate::resume(child.child.as_mut().expect("owned child"))
            }
            #[cfg(target_os = "linux")]
            Self::Gated(child) => child.resume(),
        }
    }
}

pub(super) enum PresentationPolicy {
    PreserveEvidence,
    CancelOnTransportFailure,
}
pub(super) struct CapturedOutput {
    pub observation: crate::process_termination::Observation,
    pub output: Output,
    pub output_error: Option<String>,
    pub capture_error: Option<String>,
}

/// Best-effort terminal presentation is independent from capture custody.
/// It must never delay, cancel, or alter the agent execution.
pub(super) use crate::workloads::session::ExecutionProgress as Progress;

trait Pipe: Read {
    fn prepare(&self) -> io::Result<()>;
    fn available(&self) -> io::Result<Option<usize>>;
}
#[cfg(unix)]
impl<T: Read + std::os::fd::AsRawFd> Pipe for T {
    fn prepare(&self) -> io::Result<()> {
        let fd = self.as_raw_fd();
        // SAFETY: only the newly owned output pipe's descriptor is changed.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn available(&self) -> io::Result<Option<usize>> {
        Ok(None)
    }
}
#[cfg(windows)]
impl<T: Read + std::os::windows::io::AsRawHandle> Pipe for T {
    fn prepare(&self) -> io::Result<()> {
        self.available().map(|_| ())
    }
    fn available(&self) -> io::Result<Option<usize>> {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn PeekNamedPipe(
                handle: *mut std::ffi::c_void,
                buffer: *mut std::ffi::c_void,
                size: u32,
                read: *mut u32,
                available: *mut u32,
                left: *mut u32,
            ) -> i32;
        }
        let mut available = 0;
        // SAFETY: std owns this anonymous pipe; no competing reader exists.
        // Peek works for anonymous pipes and never waits for incoming bytes.
        let ok = unsafe {
            PeekNamedPipe(
                self.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 {
            return Ok(Some(available as usize));
        }
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(109 | 232)) {
            Ok(Some(usize::MAX))
        } else {
            Err(error)
        }
    }
}

// std process execution is implemented by Unix and Windows hosts. Keep other
// std targets compiling and fail at pipe preparation rather than guessing a
// blocking read to be cancellable on an unimplemented platform.
#[cfg(not(any(unix, windows)))]
impl<T: Read> Pipe for T {
    fn prepare(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "nonblocking child pipes require a platform implementation",
        ))
    }
    fn available(&self) -> io::Result<Option<usize>> {
        self.prepare().map(|()| None)
    }
}

struct Stream<R> {
    source: R,
    bytes: Vec<u8>,
    eof: bool,
    failed: bool,
    truncated: bool,
    #[cfg(test)]
    is_error: bool,
}
fn append_prefix(capture: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let size = CAPTURE_LIMIT.saturating_sub(capture.len()).min(bytes.len());
    let needed = capture.len() + size;
    if needed > capture.capacity() {
        // Vec's default geometric growth can overshoot the evidence budget for
        // irregular short reads. Bound the requested capacity, not just length.
        let capacity = capture
            .capacity()
            .max(16 * 1024)
            .saturating_mul(2)
            .min(CAPTURE_LIMIT)
            .max(needed);
        capture.reserve_exact(capacity - capture.len());
    }
    capture.extend_from_slice(&bytes[..size]);
    size != bytes.len()
}
impl<R: Pipe> Stream<R> {
    fn new(source: R, _is_error: bool) -> io::Result<Self> {
        source.prepare()?;
        Ok(Self {
            source,
            bytes: Vec::new(),
            eof: false,
            failed: false,
            truncated: false,
            #[cfg(test)]
            is_error: _is_error,
        })
    }
    fn read_batch(&mut self, mut present: impl FnMut(&[u8]) -> io::Result<()>) -> io::Result<bool> {
        if self.eof || self.failed {
            return Ok(false);
        }
        let mut buffer = [0; 16 * 1024];
        let mut consumed = 0;
        let mut attempts = 0;
        while consumed < READ_BATCH && attempts < 8 {
            attempts += 1;
            let available = match self.source.available() {
                Ok(value) => value,
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            };
            if available == Some(0) {
                break;
            }
            #[cfg(windows)]
            if available == Some(usize::MAX) {
                self.eof = true;
                break;
            }
            let amount = available.unwrap_or(buffer.len()).min(buffer.len());
            #[cfg(test)]
            let injected = test_support::read_fault(self.is_error, self.bytes.len());
            #[cfg(not(test))]
            let injected: Option<io::Error> = None;
            let read = match injected {
                Some(error) => Err(error),
                None => self.source.read(&mut buffer[..amount]),
            };
            match read {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(size) => {
                    consumed += size;
                    self.truncated |= append_prefix(&mut self.bytes, &buffer[..size]);
                    present(&buffer[..size])?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    self.failed = true;
                    return Err(error);
                }
            }
        }
        Ok(consumed != 0)
    }
}

pub(super) fn capture_with_output_streams(
    child: OwnedChild,
    show_output: bool,
    output: &mut dyn Write,
    error_output: Option<&mut dyn Write>,
    runtime: Duration,
    policy: PresentationPolicy,
    after_exit: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<CapturedOutput> {
    capture_with_output_streams_progress(
        child,
        show_output,
        output,
        error_output,
        runtime,
        policy,
        None,
        after_exit,
    )
}

// Progress is best-effort presentation and intentionally remains outside the
// bounded capture/presentation policy object.
#[allow(clippy::too_many_arguments)]
pub(super) fn capture_with_output_streams_progress(
    mut child: OwnedChild,
    show_output: bool,
    output: &mut dyn Write,
    error_output: Option<&mut dyn Write>,
    runtime: Duration,
    policy: PresentationPolicy,
    mut progress: Option<&mut dyn Progress>,
    after_exit: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<CapturedOutput> {
    let result = capture_inner(
        &mut child,
        show_output,
        output,
        error_output,
        runtime,
        policy,
        &mut progress,
        after_exit,
    );
    if let Some(progress) = progress {
        progress.finish();
    }
    result.map_err(|primary| match child.stop() {
        Ok(_) => primary,
        Err(cleanup) => io::Error::other(format!(
            "{primary}; owned child cleanup also failed: {cleanup}"
        )),
    })
}

#[allow(clippy::too_many_arguments)]
fn capture_inner(
    child: &mut OwnedChild,
    mut show_output: bool,
    output: &mut dyn Write,
    mut error_output: Option<&mut dyn Write>,
    runtime: Duration,
    policy: PresentationPolicy,
    progress_display: &mut Option<&mut dyn Progress>,
    after_exit: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<CapturedOutput> {
    let stdout = child
        .take_stdout()
        .ok_or_else(|| io::Error::other("agent stdout was not piped"))?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| io::Error::other("agent stderr was not piped"))?;
    let mut stdout = Stream::new(stdout, false)?;
    let mut stderr = Stream::new(stderr, true)?;
    let execution_deadline = Instant::now() + runtime;
    let mut drain_deadline = None;
    let mut status = None;
    let mut output_error = None;
    let mut capture_error = None;
    let mut timed_out = false;
    loop {
        if status.is_none() {
            status = child.try_wait()?;
            if status.is_none() && Instant::now() >= execution_deadline {
                timed_out = true;
                status = Some(child.stop()?);
            }
            if status.is_some() {
                // Freeze the caller's execution clock before callbacks/draining.
                after_exit()?;
                drain_deadline = Some(Instant::now() + DRAIN);
            }
        }
        let mut progress = false;
        for (is_error, reader) in [
            (false, &mut stdout as &mut dyn StreamRead),
            (true, &mut stderr as &mut dyn StreamRead),
        ] {
            let mut present = |bytes: &[u8]| {
                if !show_output {
                    return Ok(());
                }
                let written = if is_error {
                    if let Some(writer) = error_output.as_deref_mut() {
                        writer.write_all(bytes).and_then(|()| writer.flush())
                    } else {
                        output.write_all(bytes).and_then(|()| output.flush())
                    }
                } else {
                    output.write_all(bytes).and_then(|()| output.flush())
                };
                if let Err(error) = written {
                    if matches!(policy, PresentationPolicy::CancelOnTransportFailure) {
                        return Err(error);
                    }
                    output_error = Some(format!("cannot write agent output: {error}"));
                    show_output = false;
                }
                Ok(())
            };
            match reader.batch(&mut present) {
                Ok(read) => progress |= read,
                Err(error) => {
                    if matches!(policy, PresentationPolicy::CancelOnTransportFailure) {
                        return Err(error);
                    }
                    capture_error.get_or_insert_with(|| {
                        format!(
                            "agent {} capture failed: {error}",
                            if is_error { "stderr" } else { "stdout" }
                        )
                    });
                }
            }
        }
        if stdout.truncated || stderr.truncated {
            capture_error.get_or_insert_with(|| format!("agent output exceeded the {CAPTURE_LIMIT}-byte per-stream capture limit; transcript evidence is incomplete"));
        }
        if let Some(progress) = progress_display.as_deref_mut() {
            progress.update(&stdout.bytes, &stderr.bytes);
        }
        // A failed pipe cannot drain a live writer safely. Stop the owned child
        // and retain partial evidence; the native caller still grades once.
        if status.is_none() && (stdout.failed || stderr.failed) {
            status = Some(child.stop()?);
            after_exit()?;
            drain_deadline = Some(Instant::now() + DRAIN);
        }
        if status.is_some() && (stdout.eof || stdout.failed) && (stderr.eof || stderr.failed) {
            break;
        }
        if drain_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            capture_error.get_or_insert_with(|| "agent output drain deadline exceeded after exit; transcript evidence is incomplete".into());
            break;
        }
        if !progress {
            std::thread::sleep(TICK);
        }
    }
    if timed_out {
        let message = format!(
            "agent execution exceeded {} seconds\n",
            runtime.as_secs_f64()
        );
        append_prefix(&mut stderr.bytes, message.as_bytes());
    }
    Ok(CapturedOutput {
        observation: child.observation(),
        output: Output {
            status: status.expect("exit observed before settlement"),
            stdout: stdout.bytes,
            stderr: stderr.bytes,
        },
        output_error,
        capture_error,
    })
}

trait StreamRead {
    fn batch(&mut self, present: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<bool>;
}
impl<R: Pipe> StreamRead for Stream<R> {
    fn batch(&mut self, present: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<bool> {
        self.read_batch(present)
    }
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    std::thread_local! { static FAIL_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    std::thread_local! { static STOP_FAULT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) }; }
    pub(super) fn stop_poll() -> Option<io::Result<Option<ExitStatus>>> {
        STOP_FAULT.get().map(|fail| {
            if fail {
                Err(io::Error::other("fixture cleanup polling failure"))
            } else {
                Ok(None)
            }
        })
    }
    pub(super) fn read_fault(is_error: bool, length: usize) -> Option<io::Error> {
        (!is_error && length > 0 && FAIL_READ.replace(false))
            .then(|| io::Error::other("fixture permanent output read failure"))
    }
    pub(crate) struct Scope;
    impl Scope {
        pub(crate) fn read_failure() -> Self {
            assert!(!FAIL_READ.replace(true));
            Self
        }
        pub(super) fn stop_fault(fail: bool) -> Self {
            assert!(STOP_FAULT.replace(Some(fail)).is_none());
            Self
        }
    }
    impl Drop for Scope {
        fn drop(&mut self) {
            FAIL_READ.set(false);
            STOP_FAULT.set(None);
        }
    }
}
