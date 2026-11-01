//! Linux producer custody. File logging never owns cancellation authority.
#![allow(unsafe_code)] // fcntl only configures owned pipe descriptors, never PIDs.
use super::*;
use crate::process_gate::{Admission, ManagedGate};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::process::Stdio;
use std::sync::{Mutex, mpsc};

const CLEANUP: Duration = Duration::from_secs(6);
const LOG_BYTES: usize = 16 * 1024 * 1024;
/// A normal private-file flush should settle promptly. This remains bounded so
/// an ancillary writer can never delay installer cancellation.
const LOG_FINISH_WAIT: Duration = Duration::from_millis(100);
const CHUNK: usize = 8192;
const OPERATION_LOG_BYTES: usize = 64 * 1024 * 1024;
const OPERATION_LOG_WORKERS: usize = 4;

pub(super) struct LogBudget {
    active: std::sync::atomic::AtomicUsize,
    remaining: std::sync::atomic::AtomicUsize,
    workers: usize,
}
impl Default for LogBudget {
    fn default() -> Self {
        Self {
            active: std::sync::atomic::AtomicUsize::new(0),
            remaining: std::sync::atomic::AtomicUsize::new(OPERATION_LOG_BYTES),
            workers: OPERATION_LOG_WORKERS,
        }
    }
}
impl LogBudget {
    #[cfg(test)]
    pub(super) fn with_limits(workers: usize, bytes: usize) -> Self {
        Self {
            active: std::sync::atomic::AtomicUsize::new(0),
            remaining: std::sync::atomic::AtomicUsize::new(bytes),
            workers,
        }
    }
    fn acquire(self: &Arc<Self>) -> Option<LogWriterPermit> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.workers).then_some(active + 1)
            })
            .ok()?;
        Some(LogWriterPermit(Arc::clone(self)))
    }
    fn reserve(&self, maximum: usize) -> usize {
        self.remaining
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                Some(remaining.saturating_sub(maximum))
            })
            .unwrap()
            .min(maximum)
    }
    fn return_unaccepted(&self, bytes: usize) {
        // Only a SyncSender rejection proves these bytes were never accepted.
        // Written, queued, or otherwise uncertain accepted data is never refunded.
        self.remaining.fetch_add(bytes, Ordering::AcqRel);
    }
}
struct LogWriterPermit(Arc<LogBudget>);
impl Drop for LogWriterPermit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
pub(super) type WriterFactory = Box<dyn FnOnce(File) -> Box<dyn Write + Send> + Send>;
#[cfg(test)]
pub(super) type ReaderFactory =
    Box<dyn FnOnce(Box<dyn Read + Send>) -> Box<dyn Read + Send> + Send>;

#[cfg(test)]
pub(super) struct AdmissionPause {
    pub(super) reached: Arc<AtomicBool>,
    pub(super) release: Arc<AtomicBool>,
    pub(super) settled: Arc<AtomicBool>,
    pub(super) client_marker: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LogReport {
    pub(crate) path: Option<PathBuf>,
    pub(crate) retained: usize,
    pub(crate) accepted: usize,
    pub(crate) discarded: usize,
    pub(crate) fault: Option<LogFault>,
    pub(crate) settled: bool,
    pub(crate) truncated: bool,
}

struct LogSink {
    queue: Option<mpsc::SyncSender<Vec<u8>>>,
    completion: Option<mpsc::Receiver<()>>,
    report: Arc<Mutex<LogReport>>,
    budget: Arc<LogBudget>,
}
impl LogSink {
    #[cfg(test)]
    fn new(root: &Path, #[cfg(test)] factory: Option<WriterFactory>) -> Self {
        Self::new_inner(
            root,
            Arc::new(LogBudget::default()),
            #[cfg(test)]
            factory,
            #[cfg(test)]
            None,
            true,
        )
    }
    fn new_inner(
        root: &Path,
        budget: Arc<LogBudget>,
        #[cfg(test)] factory: Option<WriterFactory>,
        #[cfg(test)] worker_start_error: Option<io::ErrorKind>,
        retain: bool,
    ) -> Self {
        let report = Arc::new(Mutex::new(LogReport::default()));
        if !retain {
            report.lock().unwrap().settled = true;
            return Self {
                queue: None,
                completion: None,
                report,
                budget,
            };
        }
        // Admit a bounded ancillary writer BEFORE any namespace or thread IO.
        let Some(permit) = budget.acquire() else {
            report.lock().unwrap().fault = Some(LogFault {
                phase: LogPhase::Budget,
                kind: io::ErrorKind::WouldBlock,
            });
            report.lock().unwrap().settled = true;
            return Self {
                queue: None,
                completion: None,
                report,
                budget,
            };
        };
        let prepared = private_log(root);
        let (file, path) = match prepared {
            Ok(value) => value,
            Err(error) => {
                let mut state = report.lock().unwrap();
                state.path = Some(error.path);
                state.fault = Some(error.fault);
                state.settled = true; // No writer was admitted, not a pending flush.
                drop(state);
                return Self {
                    queue: None,
                    completion: None,
                    report,
                    budget,
                };
            }
        };
        report.lock().unwrap().path = Some(path);
        let (queue, receive) = mpsc::sync_channel::<Vec<u8>>(16);
        let (completion_send, completion) = mpsc::sync_channel(1);
        let state = Arc::clone(&report);
        let spawn = move || {
            std::thread::Builder::new()
                .name("asb-install-log".into())
                .spawn(move || {
                    // This thread owns only the log file, never the gate or source stage.
                    // An uninterruptible regular-file write cannot block cancellation.
                    #[cfg(test)]
                    let mut file: Box<dyn Write + Send> = match factory {
                        Some(factory) => factory(file),
                        None => Box::new(file),
                    };
                    #[cfg(not(test))]
                    let mut file = file;
                    let mut failed = false;
                    for bytes in receive {
                        let mut offset = 0;
                        let mut interruptions = 0;
                        while !failed && offset < bytes.len() {
                            match file.write(&bytes[offset..]) {
                                Ok(0) => {
                                    state.lock().unwrap().fault.get_or_insert(LogFault {
                                        phase: LogPhase::Write,
                                        kind: io::ErrorKind::WriteZero,
                                    });
                                    failed = true;
                                }
                                Ok(count) => {
                                    offset += count;
                                    interruptions = 0;
                                    state.lock().unwrap().retained += count;
                                }
                                Err(error)
                                    if error.kind() == io::ErrorKind::Interrupted
                                        && interruptions < 32 =>
                                {
                                    interruptions += 1;
                                }
                                Err(error) => {
                                    state.lock().unwrap().fault.get_or_insert(LogFault {
                                        phase: LogPhase::Write,
                                        kind: error.kind(),
                                    });
                                    failed = true;
                                }
                            }
                        }
                        let mut report = state.lock().unwrap();
                        report.discarded = report.discarded.saturating_add(bytes.len() - offset);
                    }
                    if !failed && let Err(error) = file.flush() {
                        state.lock().unwrap().fault.get_or_insert(LogFault {
                            phase: LogPhase::Flush,
                            kind: error.kind(),
                        });
                    }
                    drop(file);
                    drop(permit); // Actual writer/flush/close ended, not just a snapshot.
                    state.lock().unwrap().settled = true;
                    let _ = completion_send.send(());
                })
        };
        #[cfg(test)]
        let spawned = worker_start_error.map_or_else(spawn, |kind| Err(io::Error::from(kind)));
        #[cfg(not(test))]
        let spawned = spawn();
        if let Err(error) = spawned {
            let mut state = report.lock().unwrap();
            state.fault.get_or_insert(LogFault {
                phase: LogPhase::WorkerStart,
                kind: error.kind(),
            });
            state.settled = true;
            drop(state);
            return Self {
                queue: None,
                completion: None,
                report,
                budget,
            };
        }
        Self {
            queue: Some(queue),
            completion: Some(completion),
            report,
            budget,
        }
    }
    fn push(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut report = self.report.lock().unwrap();
        if report.truncated || self.queue.is_none() || report.fault.is_some() {
            report.truncated = true;
            report.discarded = report.discarded.saturating_add(bytes.len());
            return;
        }
        let maximum = bytes.len().min(LOG_BYTES.saturating_sub(report.accepted));
        let count = self.budget.reserve(maximum);
        if count == 0 {
            report.fault.get_or_insert(LogFault {
                phase: LogPhase::Budget,
                kind: io::ErrorKind::WouldBlock,
            });
            report.truncated = true;
            report.discarded = report.discarded.saturating_add(bytes.len());
            return;
        }
        // Only bounded in-memory queue operations run under this short lock.
        match self
            .queue
            .as_ref()
            .unwrap()
            .try_send(bytes[..count].to_vec())
        {
            Ok(()) => {
                report.accepted += count;
                report.discarded = report.discarded.saturating_add(bytes.len() - count);
                report.truncated |= count != bytes.len();
                if count != bytes.len() {
                    report.fault.get_or_insert(LogFault {
                        phase: LogPhase::Budget,
                        kind: io::ErrorKind::WouldBlock,
                    });
                }
            }
            Err(mpsc::TrySendError::Full(_)) => {
                self.budget.return_unaccepted(count);
                report.fault.get_or_insert(LogFault {
                    phase: LogPhase::Queue,
                    kind: io::ErrorKind::WouldBlock,
                });
                report.discarded = report.discarded.saturating_add(bytes.len());
                report.truncated = true;
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.budget.return_unaccepted(count);
                report.fault.get_or_insert(LogFault {
                    phase: LogPhase::Queue,
                    kind: io::ErrorKind::BrokenPipe,
                });
                report.truncated = true;
                report.discarded = report.discarded.saturating_add(bytes.len());
            }
        }
    }
    fn finish(mut self) -> LogReport {
        self.queue.take();
        // Wait briefly for an ordinary close/flush. Never join a potentially
        // blocked regular-file writer: after this bound the report still
        // distinguishes pending diagnostics from complete capture.
        if let Some(completion) = self.completion.take() {
            let _ = completion.recv_timeout(LOG_FINISH_WAIT);
        }
        self.report.lock().unwrap().clone()
    }
}

#[derive(Debug)]
struct LogPreparationFailure {
    fault: LogFault,
    path: PathBuf,
}
fn private_log(root: &Path) -> Result<(File, PathBuf), LogPreparationFailure> {
    let attempt = format!(".asb-install-step-{}", crate::tools::runtime::unique_id());
    private_log_at(root, &attempt).map_err(|fault| LogPreparationFailure {
        fault,
        path: root.join(attempt).join("output.log"),
    })
}

fn private_log_at(root: &Path, attempt: &str) -> Result<(File, PathBuf), LogFault> {
    private_log_prepared(
        root,
        attempt,
        #[cfg(test)]
        None,
    )
}
#[cfg(test)]
type BeforeLogOpen = Box<dyn FnOnce(&Path)>;
fn private_log_prepared(
    root: &Path,
    attempt: &str,
    #[cfg(test)] before_open: Option<BeforeLogOpen>,
) -> Result<(File, PathBuf), LogFault> {
    let namespace = |error: io::Error| LogFault {
        phase: LogPhase::Namespace,
        kind: error.kind(),
    };
    // Walk each existing directory without symlink following. The new private
    // namespace is unique and create-new; foreign logs are never touched.
    let root = if root.is_absolute() {
        root.to_owned()
    } else {
        std::env::current_dir().map_err(namespace)?.join(root)
    };
    let mut current = PathBuf::from("/");
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&current)
        .map_err(namespace)?;
    for component in root.components() {
        use std::path::Component;
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => current.push(name),
            _ => {
                return Err(namespace(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "installer project path must not contain parent traversal",
                )));
            }
        }
        directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(
                PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
                    .join(component.as_os_str()),
            )
            .map_err(namespace)?;
    }
    let parent = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(parent.join(attempt))
        .map_err(namespace)?;
    let pinned = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(parent.join(attempt))
        .map_err(namespace)?;
    #[cfg(test)]
    if let Some(before_open) = before_open {
        before_open(&root.join(attempt).join("output.log"));
    }
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(PathBuf::from(format!("/proc/self/fd/{}", pinned.as_raw_fd())).join("output.log"))
        .map_err(|error| LogFault {
            phase: LogPhase::Open,
            kind: error.kind(),
        })?;
    Ok((log, root.join(attempt).join("output.log")))
}

struct Pipe {
    reader: Box<dyn Read + Send>,
    output: Vec<u8>,
    ended: bool,
    failure: Option<io::ErrorKind>,
}
impl Pipe {
    fn read_turn(
        &mut self,
        capture: Capture,
        other_bytes: usize,
        log: &LogSink,
    ) -> Result<(), AbortCause> {
        if self.ended {
            return Ok(());
        }
        let mut buffer = [0; CHUNK];
        for _ in 0..8 {
            match self.reader.read(&mut buffer) {
                Ok(0) => {
                    self.ended = true;
                    return Ok(());
                }
                Ok(count) => {
                    log.push(&buffer[..count]);
                    if let Capture::Inspect {
                        stream_bytes,
                        total_bytes,
                    } = capture
                    {
                        if count > stream_bytes.saturating_sub(self.output.len())
                            || count
                                > total_bytes
                                    .saturating_sub(self.output.len().saturating_add(other_bytes))
                        {
                            return Err(AbortCause::CaptureOverflow);
                        }
                        self.output.extend_from_slice(&buffer[..count]);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => {
                    self.failure.get_or_insert(error.kind());
                    self.ended = true;
                    return Err(AbortCause::CaptureRead);
                }
            }
        }
        Ok(())
    }
}

fn capture_failure(pipes: &[Pipe; 2]) -> Option<String> {
    pipes
        .iter()
        .find_map(|pipe| pipe.failure)
        .map(|kind| format!("required capture pipe read/configuration failed ({kind:?})"))
}

fn nonblocking(file: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: the descriptor is borrowed from a live, uniquely owned pipe.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn run(
    step: PreparedStep,
    lease: ProducerLease,
    ticket: AdmissionTicket,
) -> InstallResult<SettledStep> {
    let (deadline, limit_key) = match step.context.step_deadline(step.purpose) {
        Ok(value) => value,
        Err(error) => {
            ticket.settle(ProducerProof::NoNewAdmission);
            return Err(error);
        }
    };
    if step.context.cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
        let cause = if Instant::now() >= deadline {
            AbortCause::Deadline
        } else {
            AbortCause::Cancelled
        };
        ticket.poison();
        ticket.settle(ProducerProof::NoNewAdmission);
        return Err(InstallFailure::Abort(abort(
            &step, limit_key, cause, true, true, None,
        )));
    }
    let cancel = Arc::clone(&step.context.cancel);
    let fallback = abort(
        &step,
        limit_key,
        AbortCause::Worker,
        false,
        false,
        lease.evidence.clone(),
    );
    let (send, receive) = mpsc::sync_channel(1);
    // The thread exists before Admission.spawn. If allocation fails, no target
    // has been admitted and no producer can hold a descriptor into the stage.
    let worker_ticket = ticket.clone();
    #[cfg(test)]
    let fail_worker_start = step.fail_worker_start;
    #[cfg(not(test))]
    let fail_worker_start: Option<io::ErrorKind> = None;
    let spawned = if let Some(kind) = fail_worker_start {
        Err(io::Error::from(kind))
    } else {
        std::thread::Builder::new()
            .name("asb-install-process".into())
            .spawn(move || supervise(step, lease, deadline, limit_key, send, worker_ticket))
    };
    spawned.map_err(|error| {
        ticket.settle(ProducerProof::NoNewAdmission);
        crate::Error::Io(format!(
            "cannot establish installation custody worker: {}",
            error.kind()
        ))
    })?;
    let mut cancellation_end = None;
    loop {
        match receive.recv_timeout(Duration::from_millis(25)) {
            Ok(result) => {
                // A physical required-capture/custody abort ends this shared
                // operation, not just this owner lane. Ordinary known negatives
                // and invalid authority-token rejections do not take this path.
                if matches!(&result, Err(InstallFailure::Abort(_))) {
                    cancel.store(true, Ordering::Release);
                }
                return result;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                cancel.store(true, Ordering::Release);
                ticket.poison();
                return Err(InstallFailure::Abort(fallback));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
            cancel.store(true, Ordering::Release);
            let end = *cancellation_end.get_or_insert_with(|| Instant::now() + CLEANUP);
            if Instant::now() >= end {
                ticket.poison();
                return Err(InstallFailure::Abort(AbortInstallation {
                    cause: if Instant::now() >= deadline {
                        AbortCause::Deadline
                    } else {
                        AbortCause::Cancelled
                    },
                    ..fallback
                }));
            }
        }
    }
}

fn abort(
    step: &PreparedStep,
    key: &'static str,
    cause: AbortCause,
    cleanup: bool,
    capture: bool,
    evidence: Option<PathBuf>,
) -> AbortInstallation {
    AbortInstallation {
        cause,
        cleanup_confirmed: cleanup,
        capture_complete: capture,
        label: step.label.clone(),
        config_path: step.context.config_path.clone(),
        limit_key: key,
        evidence,
        detail: None,
    }
}

fn supervise(
    mut step: PreparedStep,
    lease: ProducerLease,
    deadline: Instant,
    key: &'static str,
    send: mpsc::SyncSender<InstallResult<SettledStep>>,
    ticket: AdmissionTicket,
) {
    // These resources remain outside every fallible/unwinding pump turn.
    let _lease = lease;
    let log = LogSink::new_inner(
        &step.context.root,
        Arc::clone(&step.context.log_budget),
        #[cfg(test)]
        step.log_writer.take(),
        #[cfg(test)]
        None,
        step.retain_diagnostics,
    );
    #[cfg(test)]
    let replacement = step.observation.take();
    #[cfg(not(test))]
    let replacement = None;
    let mut observation = match super::observer::Observer::start(&_lease, replacement) {
        Ok(observation) => observation,
        Err(error) => {
            ticket.settle(ProducerProof::NoNewAdmission);
            let _ = send.send(Err(crate::Error::Io(format!(
                "cannot establish installation resource observer: {}",
                error.kind()
            ))
            .into()));
            return;
        }
    };
    loop {
        observation.poll();
        let reason = interrupted(&step.context, deadline)
            .or_else(|| observation.failure().map(|_| AbortCause::OwnerObservation));
        if let Some(reason) = reason {
            observation.request_stop();
            ticket.poison();
            ticket.settle(ProducerProof::NoNewAdmission);
            let report = log.finish();
            step.context.record_log(&report);
            let failure = abort(
                &step,
                key,
                reason,
                true,
                false,
                _lease.evidence.clone().or(report.path),
            )
            .detail(observation.failure().map(str::to_owned));
            let _ = send.send(Err(InstallFailure::Abort(failure)));
            return; // No client was exposed; a blocked observer retains its lease.
        }
        if observation.ready() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    if let Some(reason) = interrupted(&step.context, deadline) {
        ticket.poison();
        ticket.settle(ProducerProof::NoNewAdmission);
        let evidence = _lease
            .evidence
            .clone()
            .or_else(|| log.report.lock().unwrap().path.clone());
        let _ = send.send(Err(InstallFailure::Abort(abort(
            &step, key, reason, true, true, evidence,
        ))));
        return; // No guardian/target was admitted; no invented exit status.
    }
    step.command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let lock_fds: Vec<_> = step
        .context
        .destination_owners
        .iter()
        .map(|(_, owner)| owner.raw_fd())
        .collect();
    if let Err(error) = crate::process_gate::retain_install_locks(&mut step.command, &lock_fds) {
        ticket.settle(ProducerProof::NoNewAdmission);
        let _ = send.send(Err(crate::Error::Io(format!(
            "cannot transfer installer ownership to guardian: {error}"
        ))
        .into()));
        return;
    }
    let admitted = step
        .route
        .take()
        .expect("one admission route")
        .validate_gate(&mut step.command)
        .and_then(Admission::new)
        .and_then(Admission::spawn);
    let mut gate: ManagedGate = match admitted {
        Ok(gate) => gate,
        Err(error) => {
            ticket.settle(ProducerProof::NoNewAdmission);
            let _ = send.send(Err(crate::Error::Io(format!(
                "cannot start installation step {}: {}",
                step.label,
                error.kind()
            ))
            .into()));
            return;
        }
    };
    let mut cause = None;
    let stdout = gate.take_stdout().expect("configured stdout pipe");
    let stderr = gate.take_stderr().expect("configured stderr pipe");
    let configuration_failure = nonblocking(&stdout)
        .and_then(|_| nonblocking(&stderr))
        .err()
        .map(|error| error.kind());
    let unusable = configuration_failure.is_some();
    if unusable {
        cause = Some(AbortCause::CaptureRead);
    }
    let stdout: Box<dyn Read + Send> = Box::new(stdout);
    #[cfg(test)]
    let stdout = match step.reader_factory.take() {
        Some(factory) => factory(stdout),
        None => stdout,
    };
    let mut pipes = [
        Pipe {
            reader: stdout,
            output: Vec::new(),
            ended: unusable,
            failure: configuration_failure,
        },
        Pipe {
            reader: Box::new(stderr),
            output: Vec::new(),
            ended: unusable,
            failure: configuration_failure,
        },
    ];
    #[cfg(test)]
    if let Some(pause) = &step.before_continue {
        if gate.wait_stopped().is_err() {
            cause = Some(AbortCause::Custody);
        }
        pause.reached.store(true, Ordering::Release);
        while !pause.release.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    if cause.is_none() {
        let _resumed = match gate.resume_when(|| {
            cause = interrupted(&step.context, deadline);
            cause.is_none()
        }) {
            Ok(resumed) => resumed,
            Err(_) => {
                cause = Some(AbortCause::Custody);
                false
            }
        };
        #[cfg(test)]
        if _resumed && let Some(pause) = &step.before_continue {
            // An observation-only scheduling hook ensures an already resumed
            // target can publish its marker before cancellation is polled.
            let until = Instant::now() + Duration::from_secs(2);
            while !pause.client_marker.is_file() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
    let mut status = None;
    let mut cleanup_deadline = None;
    let mut observation_deadline = None;
    let mut sent = false;
    loop {
        observation.poll();
        if observation.failure().is_some() {
            cause.get_or_insert(AbortCause::OwnerObservation);
        }
        if Instant::now() >= deadline {
            cause.get_or_insert(AbortCause::Deadline);
        }
        if step.context.cancel.load(Ordering::Acquire) {
            cause.get_or_insert(AbortCause::Cancelled);
        }
        let turn = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for index in 0..2 {
                let other = pipes[1 - index].output.len();
                if let Err(error) = pipes[index].read_turn(step.capture, other, &log) {
                    cause.get_or_insert(error);
                }
            }
        }));
        if turn.is_err() {
            cause.get_or_insert(AbortCause::Worker);
        }
        #[cfg(test)]
        let hold_observation = step
            .hold_observation
            .as_ref()
            .is_some_and(|hold| hold.load(Ordering::Acquire));
        #[cfg(not(test))]
        let hold_observation = false;
        match if hold_observation {
            Ok(None)
        } else {
            gate.try_wait()
        } {
            Ok(Some(exit)) => status = Some(exit),
            Ok(None) => {}
            Err(_) => {
                cause.get_or_insert(AbortCause::Custody);
            }
        }
        if status.is_some() && pipes.iter().all(|pipe| pipe.ended) {
            #[cfg(test)]
            if let Some(pause) = &step.before_continue {
                pause.settled.store(true, Ordering::Release);
            }
            observation.request_stop();
            let end = *observation_deadline.get_or_insert_with(|| Instant::now() + CLEANUP);
            if !observation.finished() && Instant::now() >= end {
                cause.get_or_insert(AbortCause::OwnerObservation);
                cleanup_deadline.get_or_insert(end);
            }
        }
        if let Some(reason) = cause {
            let end = *cleanup_deadline.get_or_insert_with(|| Instant::now() + CLEANUP);
            if status.is_none() {
                let _ = gate.request_cancel();
            }
            if Instant::now() >= end && !sent {
                ticket.poison();
                let evidence = _lease
                    .evidence
                    .clone()
                    .or_else(|| log.report.lock().unwrap().path.clone());
                step.context.record_log(&log.report.lock().unwrap().clone());
                let _ = send.send(Err(InstallFailure::Abort(abort(
                    &step,
                    key,
                    reason,
                    status.is_some(),
                    false,
                    evidence,
                ).detail(observation.failure().map(str::to_owned).or_else(|| capture_failure(&pipes)).or_else(||
                    (!observation.finished()).then(|| "required resource observation remains pending; its owned worker retains staging".into()))))));
                sent = true;
                // Keep exact gate, pumps, and the actual producer dependency in
                // this existing worker until proof, regardless of parent wait.
            }
        }
        if let Some(status) = status
            && pipes.iter().all(|pipe| pipe.ended)
            && observation.finished()
        {
            if cause.is_some() {
                ticket.poison();
            }
            ticket.settle(ProducerProof::Quiescent);
            if !sent {
                let log = log.finish();
                step.context.record_log(&log);
                let result = match cause {
                    Some(cause) => Err(InstallFailure::Abort(
                        abort(
                            &step,
                            key,
                            cause,
                            true,
                            false,
                            _lease.evidence.clone().or(log.path),
                        )
                        .detail(
                            observation
                                .failure()
                                .map(str::to_owned)
                                .or_else(|| capture_failure(&pipes)),
                        ),
                    )),
                    None => {
                        let [stdout, stderr] = pipes;
                        Ok(SettledStep {
                            status,
                            stdout: stdout.output,
                            stderr: stderr.output,
                            log,
                            lease: _lease,
                        })
                    }
                };
                let _ = send.send(result);
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(if sent { 25 } else { 2 }));
    }
}

fn interrupted(context: &OperationContext, deadline: Instant) -> Option<AbortCause> {
    if Instant::now() >= deadline {
        Some(AbortCause::Deadline)
    } else if context.cancel.load(Ordering::Acquire) {
        Some(AbortCause::Cancelled)
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
