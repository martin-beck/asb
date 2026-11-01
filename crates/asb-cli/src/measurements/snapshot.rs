//! Cancellable, bounded observations; the worker retains any uncertain custody.
use super::{ProcessIdentity, TreeSnapshotArguments, measurement_interval_ms, process_tree_pids};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub(super) struct ProcessTreeSnapshot {
    stop: Arc<AtomicBool>,
    handle: Option<WorkerHandle>,
}
type WorkerTask = Box<dyn FnOnce() -> Result<(), String> + Send>;
type WorkerHandle = std::thread::JoinHandle<Result<(), String>>;
fn spawn_worker(task: WorkerTask) -> io::Result<WorkerHandle> {
    std::thread::Builder::new()
        .name("asb-process-tree-snapshot".into())
        .spawn(task)
}
impl ProcessTreeSnapshot {
    pub(super) fn start(
        root: ProcessIdentity,
        executable: PathBuf,
        arguments: TreeSnapshotArguments,
        stdout: File,
        stderr: File,
    ) -> Option<Self> {
        Self::start_with_spawn(root, executable, arguments, stdout, stderr, spawn_worker)
    }
    pub(super) fn start_with_spawn(
        root: ProcessIdentity,
        executable: PathBuf,
        arguments: TreeSnapshotArguments,
        mut stdout: File,
        mut stderr: File,
        spawn: impl FnOnce(WorkerTask) -> io::Result<WorkerHandle>,
    ) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&stop);
        let handle = spawn(Box::new(move || {
            let started = Instant::now();
            while !cancelled.load(Ordering::Acquire) && root.matches() {
                let members = process_tree_pids(root.pid)
                    .into_iter()
                    .filter_map(ProcessIdentity::read)
                    .filter(|member| member.in_tree(root))
                    .collect::<Vec<_>>();
                if !root.matches() || members.is_empty() {
                    break;
                }
                let pids = members.iter().map(|member| member.pid).collect::<Vec<_>>();
                let request = arguments(&pids);
                let Some(observation) = observe(&executable, request, &cancelled, false)? else {
                    break;
                };
                if !root.matches() || members.iter().any(|member| !member.in_tree(root)) {
                    continue;
                }
                write_observation(&mut stdout, &mut stderr, &observation, started.elapsed())?;
                let deadline = Instant::now() + Duration::from_millis(measurement_interval_ms());
                while !cancelled.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(())
        }))
        .ok()?;
        Some(Self {
            stop,
            handle: Some(handle),
        })
    }
    #[cfg(test)]
    pub(super) fn stop(mut self) -> Result<(), String> {
        self.settle()
    }
    pub(super) fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    pub(super) fn stop_before_with_owner(
        mut self,
        deadline: Instant,
        owner: Option<Arc<crate::output_lock::OutputLock>>,
    ) -> Result<(), String> {
        self.settle_before(deadline, owner)
    }
    #[cfg(test)]
    pub(super) fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(|handle| handle.is_finished())
    }
    fn settle(&mut self) -> Result<(), String> {
        self.settle_before(Instant::now() + Duration::from_secs(6), None)
    }
    fn settle_before(
        &mut self,
        deadline: Instant,
        owner: Option<Arc<crate::output_lock::OutputLock>>,
    ) -> Result<(), String> {
        self.request_stop();
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !handle.is_finished() {
            super::retain_owner_until_worker_exit(handle, owner);
            // The existing worker still owns its files and guardian. Do not
            // join it, publish a complete capture, or create another worker.
            return Err("process-tree snapshot settlement is unconfirmed; existing worker retains producer custody".into());
        }
        handle
            .join()
            .map_err(|_| "process-tree snapshot worker failed")?
    }
}
impl Drop for ProcessTreeSnapshot {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}

/// Generic command limits are supplied by the owning adapter, never inferred
/// from a collector ID in the measurement runtime.
pub(crate) struct SnapshotCommand {
    pub arguments: Vec<String>,
    pub runtime: Duration,
    pub stream_bytes: usize,
}

pub(crate) fn bounded_version(
    executable: &std::path::Path,
    argument: &str,
    budget: Duration,
) -> Option<(Vec<u8>, Vec<u8>)> {
    #[cfg(target_os = "linux")]
    {
        let request = SnapshotCommand {
            arguments: vec![argument.to_owned()],
            runtime: budget,
            stream_bytes: 8192,
        };
        let result = observe(executable, request, &AtomicBool::new(false), true).ok()??;
        Some((result.stdout, result.stderr))
    }
    #[cfg(not(target_os = "linux"))]
    {
        crate::runner::portable_version_capture(executable, argument, budget)
    }
}

struct Observation {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn write_observation(
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    observation: &Observation,
    elapsed: Duration,
) -> Result<(), String> {
    writeln!(
        stdout,
        "# asb-snapshot-elapsed-seconds {:.6}",
        elapsed.as_secs_f64()
    )
    .map_err(|error| format!("cannot write process-tree snapshot timestamp: {error}"))?;
    stdout
        .write_all(&observation.stdout)
        .map_err(|error| format!("cannot write process-tree snapshot stdout: {error}"))?;
    stderr
        .write_all(&observation.stderr)
        .map_err(|error| format!("cannot write process-tree snapshot stderr: {error}"))?;
    stdout
        .flush()
        .map_err(|error| format!("cannot flush process-tree snapshot stdout: {error}"))?;
    stderr
        .flush()
        .map_err(|error| format!("cannot flush process-tree snapshot stderr: {error}"))
}

/// One fair, bounded turn. Interrupted calls consume attempts as well as
/// checking the same cancellation/deadline; they cannot monopolize the pump.
#[cfg(target_os = "linux")]
fn read_turn(
    pipe: &mut dyn Read,
    output: &mut Vec<u8>,
    eof: &mut bool,
    limit: usize,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let mut bytes = 0;
    for _ in 0..32 {
        if *eof || bytes >= 64 * 1024 || cancelled.load(Ordering::Acquire) {
            break;
        }
        if Instant::now() >= deadline {
            return Err("process-tree snapshot output read deadline exceeded".into());
        }
        let mut buffer = [0; 4096];
        match pipe.read(&mut buffer) {
            Ok(0) => *eof = true,
            Ok(length) => {
                if output.len() + length > limit {
                    return Err("process-tree snapshot output exceeded its capture limit".into());
                }
                output.extend_from_slice(&buffer[..length]);
                bytes += length;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(format!("cannot read process-tree snapshot output: {error}")),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn runtime() -> io::Result<PathBuf> {
    #[cfg(test)]
    return Ok(std::env::current_exe()?
        .parent()
        .ok_or_else(|| io::Error::other("missing test binary parent"))?
        .parent()
        .ok_or_else(|| io::Error::other("missing actual ASB binary parent"))?
        .join("asb"));
    #[cfg(not(test))]
    crate::process_gate::executable()
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(super) fn nonblocking(pipe: &impl std::os::fd::AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn observe(
    executable: &std::path::Path,
    request: SnapshotCommand,
    cancelled: &AtomicBool,
    no_auto_install: bool,
) -> Result<Option<Observation>, String> {
    if request.runtime.is_zero()
        || request.runtime > Duration::from_secs(3600)
        || request.stream_bytes == 0
        || request.stream_bytes > 256 * 1024 * 1024
    {
        return Err("process-tree snapshot command has invalid capture limits".into());
    }
    if cancelled.load(Ordering::Acquire) {
        return Ok(None);
    }
    let mut launch = crate::launch::ProcessLaunch::new(executable);
    for argument in &request.arguments {
        launch.arg(argument);
    }
    let (mut command, route) = launch.native(Some(&runtime().map_err(|error| error.to_string())?));
    if no_auto_install {
        command.env("RUSTUP_AUTO_INSTALL", "0");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut gate = crate::process_gate::Admission::with_principal_control(
        route
            .validate_gate(&mut command)
            .map_err(|error| error.to_string())?,
    )
    .and_then(|admission| admission.spawn())
    .map_err(|error| error.to_string())?;
    let mut out = gate.take_stdout().ok_or("missing snapshot stdout")?;
    let mut err = gate.take_stderr().ok_or("missing snapshot stderr")?;
    nonblocking(&out)
        .and_then(|()| nonblocking(&err))
        .map_err(|error| error.to_string())?;
    gate.resume().map_err(|error| error.to_string())?;
    let deadline = Instant::now() + request.runtime;
    let mut output = [Vec::new(), Vec::new()];
    let mut eof = [false; 2];
    let mut exited = false;
    let mut drain = None;
    loop {
        if cancelled.load(Ordering::Acquire) {
            if !exited {
                let grace = Instant::now() + Duration::from_millis(100);
                let _ = gate.interrupt_principal(grace);
                while Instant::now() < grace {
                    if gate
                        .try_wait()
                        .map_err(|error| error.to_string())?
                        .is_some()
                    {
                        exited = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                // Delivery acknowledgement is never used as quiescence proof.
                let _acknowledged = gate.interruption_acknowledged();
                if !exited {
                    gate.stop().map_err(|error| error.to_string())?;
                }
            }
            return Ok(None);
        }
        if !exited
            && gate
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
        {
            let status = gate
                .principal_exit()
                .ok_or("snapshot principal termination is unknown")?
                .status();
            if !status.success() {
                return Err("process-tree snapshot command failed".into());
            }
            exited = true;
            drain = Some(Instant::now() + Duration::from_secs(1));
        }
        if !exited && Instant::now() >= deadline {
            return Err("process-tree snapshot command exceeded its deadline".into());
        }
        for (index, pipe) in [
            (0, &mut out as &mut dyn Read),
            (1, &mut err as &mut dyn Read),
        ] {
            read_turn(
                pipe,
                &mut output[index],
                &mut eof[index],
                request.stream_bytes,
                drain.unwrap_or(deadline),
                cancelled,
            )?;
        }
        if cancelled.load(Ordering::Acquire) {
            continue;
        }
        if exited && eof.iter().all(|eof| *eof) {
            let [out, err] = output;
            return Ok(Some(Observation {
                stdout: out,
                stderr: err,
            }));
        }
        if drain.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(
                "process-tree snapshot output remained open after confirmed tree quiescence".into(),
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod io_tests {
    use super::*;
    struct Reader<'a> {
        calls: usize,
        cancel: Option<&'a AtomicBool>,
        error: io::ErrorKind,
    }
    impl Read for Reader<'_> {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            if let Some(cancel) = self.cancel {
                cancel.store(true, Ordering::Release);
            }
            Err(io::Error::new(self.error, "fixture read failure"))
        }
    }
    #[test]
    fn interrupted_reads_yield_and_recheck_cancel_and_shared_deadline() {
        let cancelled = AtomicBool::new(false);
        let mut bytes = Vec::new();
        let mut eof = false;
        let mut reader = Reader {
            calls: 0,
            cancel: None,
            error: io::ErrorKind::Interrupted,
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        read_turn(
            &mut reader,
            &mut bytes,
            &mut eof,
            1024,
            deadline,
            &cancelled,
        )
        .unwrap();
        assert_eq!(reader.calls, 32, "Interrupted must consume a fair turn");
        assert!(!eof);
        assert!(bytes.is_empty());
        reader.cancel = Some(&cancelled);
        read_turn(
            &mut reader,
            &mut bytes,
            &mut eof,
            1024,
            deadline,
            &cancelled,
        )
        .unwrap();
        assert_eq!(reader.calls, 33, "cancellation must interrupt retry");
        cancelled.store(false, Ordering::Release);
        reader.cancel = None;
        let error = read_turn(
            &mut reader,
            &mut bytes,
            &mut eof,
            1024,
            Instant::now(),
            &cancelled,
        )
        .unwrap_err();
        assert!(error.contains("deadline"));
        assert_eq!(
            reader.calls, 33,
            "expired budget must never perform another read"
        );
    }
    #[test]
    fn permanent_read_error_cannot_be_complete_eof_even_after_a_valid_prefix() {
        let mut reader = Reader {
            calls: 0,
            cancel: None,
            error: io::ErrorKind::PermissionDenied,
        };
        let mut bytes = b"valid prefix\n".to_vec();
        let mut eof = false;
        let error = read_turn(
            &mut reader,
            &mut bytes,
            &mut eof,
            1024,
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.contains("fixture read failure"));
        assert!(!eof);
        assert_eq!(bytes, b"valid prefix\n");
    }
    #[test]
    fn actual_write_zero_is_a_failed_observation_not_silent_completion() {
        struct Zero;
        impl Write for Zero {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Ok(0)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let observation = Observation {
            stdout: b"valid prefix\n".to_vec(),
            stderr: Vec::new(),
        };
        let error = write_observation(&mut Zero, &mut Vec::new(), &observation, Duration::ZERO)
            .unwrap_err();
        assert!(error.contains("write"));
    }
}

#[cfg(not(target_os = "linux"))]
fn observe(
    _: &std::path::Path,
    _: SnapshotCommand,
    _: &AtomicBool,
    _: bool,
) -> Result<Option<Observation>, String> {
    Err("bounded process-tree snapshot supervision requires Linux native gate support".into())
}
