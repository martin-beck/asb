//! Keep an isolated workload's services alive until its evaluator is finished.
//! Only the private guest driver uses this supervisor. Native runs retain the
//! default quiescence contract. The owning SDK removes/stops the PID namespace.
#![allow(unsafe_code)]
use super::{Error, OwnedChild};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

const LIMIT: usize = 8 * 1024 * 1024;
enum Event {
    Data(bool, Vec<u8>),
    Frozen,
    Failed,
    Truncated,
}

pub(crate) struct HeldProcess {
    pub output: Output,
    pub truncated: bool,
    pub elapsed_ms: u64,
    pub timed_out: bool,
    _scope: crate::process_gate::DescendantScope,
    _readers: Vec<std::thread::JoinHandle<()>>,
}

impl HeldProcess {
    /// The SDK's namespace teardown is the terminal owner of this process.
    /// Reader threads keep inherited service output open and drain/discard it;
    /// closing those pipes at client exit could kill a valid service by SIGPIPE.
    pub(crate) fn hold_for_evaluation(self) -> ! {
        loop {
            // Services may themselves fork and exit during verification. Keep
            // adopted zombies from exhausting the private namespace's PID cap.
            let _ = self._scope.reap_exited();
            std::thread::park_timeout(Duration::from_millis(100));
        }
    }
}

fn reader<R: Read + AsRawFd>(
    mut source: R,
    stderr: bool,
    freeze: Arc<AtomicBool>,
    sender: SyncSender<Event>,
) {
    let descriptor = source.as_raw_fd();
    // SAFETY: fcntl changes only this newly owned child-output descriptor.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        let _ = sender.send(Event::Failed);
        return;
    }
    let mut buffer = [0u8; 16384];
    let mut frozen = false;
    let mut tail = 0usize;
    loop {
        let freezing = freeze.load(Ordering::Acquire);
        match source.read(&mut buffer) {
            Ok(0) => {
                if !frozen {
                    let _ = sender.send(Event::Frozen);
                }
                return;
            }
            Ok(size) => {
                if !frozen
                    && sender
                        .send(Event::Data(stderr, buffer[..size].to_vec()))
                        .is_err()
                {
                    return;
                }
                if freezing && !frozen {
                    tail += size;
                    // A continuously noisy service must not postpone the
                    // client boundary forever. Capture a bounded queued tail.
                    if tail >= 256 * 1024 {
                        let _ = sender.send(Event::Truncated);
                        let _ = sender.send(Event::Frozen);
                        frozen = true;
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if freezing && !frozen {
                    let _ = sender.send(Event::Frozen);
                    frozen = true;
                }
                let mut poll = libc::pollfd {
                    fd: descriptor,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: poll borrows one valid descriptor for at most 50 ms.
                if unsafe { libc::poll(&mut poll, 1, 50) } < 0
                    && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                {
                    let _ = sender.send(Event::Failed);
                    return;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
            Err(_) => {
                let _ = sender.send(Event::Failed);
                return;
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn start<'a>(
    command: &mut Command,
    stdout: &'a mut dyn Write,
    stderr: &'a mut dyn Write,
    runtime: Duration,
    after_exit: impl FnMut(std::process::ExitStatus, u64, bool) -> Result<(), Error>,
) -> Result<HeldProcess, Error> {
    start_with_spawn(command, stdout, stderr, runtime, |_| Ok(()), after_exit)
}

pub(crate) fn start_with_spawn<'a>(
    command: &mut Command,
    stdout: &'a mut dyn Write,
    stderr: &'a mut dyn Write,
    runtime: Duration,
    mut on_spawn: impl FnMut(u32) -> Result<(), Error>,
    mut after_exit: impl FnMut(std::process::ExitStatus, u64, bool) -> Result<(), Error>,
) -> Result<HeldProcess, Error> {
    let scope = crate::process_gate::DescendantScope::new()
        .map_err(|_| Error::Run("cannot own workload service descendants".into()))?;
    let started = Instant::now();
    let mut child = OwnedChild::spawn_direct(command)
        .map_err(|error| Error::Run(format!("cannot start workload client: {error}")))?;
    on_spawn(child.id())?;
    let out = child
        .take_stdout()
        .ok_or_else(|| Error::Run("workload client stdout is not piped".into()))?;
    let err = child
        .take_stderr()
        .ok_or_else(|| Error::Run("workload client stderr is not piped".into()))?;
    let freeze = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel(16);
    let out_freeze = Arc::clone(&freeze);
    let out_sender = sender.clone();
    let readers = vec![
        std::thread::spawn(move || reader(out, false, out_freeze, out_sender)),
        {
            let err_freeze = Arc::clone(&freeze);
            std::thread::spawn(move || reader(err, true, err_freeze, sender))
        },
    ];
    let mut output = Output {
        status: std::process::ExitStatus::from_raw(0),
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    let mut status = None;
    let mut frozen = 0;
    let mut truncated = false;
    let deadline = started + runtime;
    let mut elapsed_ms = 0;
    let mut timed_out = false;
    loop {
        let event = match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => Some(event),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) if frozen == 2 => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::Run(
                    "workload output readers stopped without completion".into(),
                ));
            }
        };
        if let Some(event) = event {
            match event {
                Event::Data(is_error, chunk) => {
                    let (capture, writer) = if is_error {
                        (&mut output.stderr, &mut *stderr)
                    } else {
                        (&mut output.stdout, &mut *stdout)
                    };
                    let room = LIMIT.saturating_sub(capture.len());
                    truncated |= chunk.len() > room;
                    capture.extend_from_slice(&chunk[..room.min(chunk.len())]);
                    writer
                        .write_all(&chunk[..room.min(chunk.len())])
                        .map_err(crate::io_error)?;
                    writer.flush().map_err(crate::io_error)?;
                }
                Event::Frozen => frozen += 1,
                Event::Truncated => truncated = true,
                Event::Failed => return Err(Error::Run("workload output reader failed".into())),
            }
        }
        if status.is_none() {
            status = child.try_wait().map_err(crate::io_error)?;
            if status.is_none() && Instant::now() >= deadline {
                timed_out = true;
                status = Some(child.stop().map_err(crate::io_error)?);
                let message = b"agent execution exceeded its time budget\n";
                stderr.write_all(message).map_err(crate::io_error)?;
                stderr.flush().map_err(crate::io_error)?;
                let room = LIMIT.saturating_sub(output.stderr.len());
                truncated |= message.len() > room;
                output
                    .stderr
                    .extend_from_slice(&message[..room.min(message.len())]);
            }
            if let Some(status) = status {
                elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
                freeze.store(true, Ordering::Release);
                after_exit(status, elapsed_ms, timed_out)?;
            }
        }
        if status.is_some() && frozen == 2 {
            break;
        }
        if frozen == 2 {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    output.status = status.expect("principal exit was observed");
    Ok(HeldProcess {
        output,
        truncated,
        elapsed_ms,
        timed_out,
        _scope: scope,
        _readers: readers,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/workload_process.rs"]
mod coverage_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn service_pipe_is_drained_after_freeze_without_closing_or_capturing_later_output() {
        let (source, mut service) = UnixStream::pair().unwrap();
        let freeze = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(16);
        let flag = Arc::clone(&freeze);
        let worker = std::thread::spawn(move || reader(source, false, flag, sender));
        service.write_all(b"client output").unwrap();
        match receiver.recv_timeout(Duration::from_secs(2)).unwrap() {
            Event::Data(false, bytes) => assert_eq!(bytes, b"client output"),
            _ => panic!("expected bounded client output"),
        }
        freeze.store(true, Ordering::Release);
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Ok(Event::Frozen)
        ));
        // A service inheriting the client's stdout still has a live reader.
        // Its later output must not contaminate frozen usage/response parsing.
        for _ in 0..100 {
            service.write_all(&[b'x'; 4096]).unwrap();
        }
        drop(service);
        worker.join().unwrap();
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn closed_output_acknowledges_completion_once_without_needing_a_freeze() {
        let (source, service) = UnixStream::pair().unwrap();
        drop(service);
        let (sender, receiver) = mpsc::sync_channel(16);
        reader(source, true, Arc::new(AtomicBool::new(false)), sender);
        assert!(matches!(receiver.try_recv(), Ok(Event::Frozen)));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }
}
