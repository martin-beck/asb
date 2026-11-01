//! Collector-independent ownership and capture facts, never metric/tool policy.
use super::snapshot::ProcessTreeSnapshot;
use std::fs::File;
use std::io::{self, Read, Write};
use std::process::{Command, ExitStatus};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

#[cfg(test)]
#[derive(Clone)]
struct Fault {
    kind: &'static str,
    reached: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}
#[cfg(test)]
thread_local! {
    static FAULT: std::cell::RefCell<Option<Fault>> = const { std::cell::RefCell::new(None) };
}

#[derive(Clone)]
pub(crate) struct Completion {
    pub exit: Option<ExitStatus>,
    pub guardian_cleanup_confirmed: Option<bool>,
    pub capture_complete: bool,
    pub interruption_requested: bool,
    pub interruption_acknowledged: bool,
    pub error: Option<String>,
}
impl Completion {
    fn failed(exit: Option<ExitStatus>, reason: impl Into<String>) -> Self {
        Self {
            exit,
            guardian_cleanup_confirmed: None,
            capture_complete: false,
            interruption_requested: false,
            interruption_acknowledged: false,
            error: Some(reason.into()),
        }
    }
    #[cfg(test)]
    pub fn test_completed(exit: ExitStatus) -> Self {
        Self {
            exit: Some(exit),
            guardian_cleanup_confirmed: None,
            capture_complete: true,
            interruption_requested: false,
            interruption_acknowledged: false,
            error: None,
        }
    }
}

/// Output and graceful-flush policy is supplied by the owning adapter.
#[derive(Clone, Copy)]
pub(crate) struct Policy {
    pub stream_bytes: u64,
    pub graceful: Option<Duration>,
}

pub(crate) fn command(
    launch: &mut crate::launch::ProcessLaunch,
) -> Result<(Command, crate::launch::CommandRoute), String> {
    if crate::tools::host_executable(std::path::Path::new(&launch.program)).is_none() {
        return Err(format!(
            "collector executable is missing or not executable: {}",
            launch.program.to_string_lossy()
        ));
    }
    #[cfg(target_os = "linux")]
    return Ok(launch.native(Some(
        &super::snapshot::runtime().map_err(|error| error.to_string())?,
    )));
    #[cfg(not(target_os = "linux"))]
    Ok(launch.native(None))
}

pub(crate) struct Producer {
    active: Option<Active>,
    completion: Option<Completion>,
}
enum Active {
    #[cfg(target_os = "linux")]
    Process(Process),
    Snapshot(ProcessTreeSnapshot),
}
impl Producer {
    pub(super) fn snapshot(snapshot: ProcessTreeSnapshot) -> Self {
        Self {
            active: Some(Active::Snapshot(snapshot)),
            completion: None,
        }
    }
    pub fn spawn_with_dependency(
        command: &mut Command,
        route: crate::launch::CommandRoute,
        policy: Policy,
        files: [File; 2],
        dependency: Option<Arc<dyn Send + Sync>>,
    ) -> Result<Self, String> {
        #[cfg(target_os = "linux")]
        {
            if policy.stream_bytes == 0
                || policy.stream_bytes > 256 * 1024 * 1024
                || policy
                    .graceful
                    .is_some_and(|grace| grace.is_zero() || grace > Duration::from_secs(60))
            {
                return Err("invalid collector capture policy".into());
            }
            command
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let gate = crate::process_gate::Admission::with_principal_control(
                route
                    .validate_gate(command)
                    .map_err(|error| error.to_string())?,
            )
            .and_then(|admission| admission.spawn())
            .map_err(|error| error.to_string())?;
            let stopped = Arc::new(AtomicBool::new(false));
            let request = Arc::clone(&stopped);
            // Allocate the worker before it can resume this stopped guardian.
            // Failed spawn drops only the unadmitted gate into existing custody.
            #[cfg(test)]
            let id = gate.id();
            #[cfg(test)]
            let fault = FAULT.with(|fault| fault.borrow_mut().take());
            let handle = std::thread::Builder::new()
                .name("asb-collector-producer".into())
                .spawn(move || {
                    capture(
                        gate,
                        files,
                        policy,
                        &request,
                        dependency,
                        #[cfg(test)]
                        fault,
                    )
                })
                .map_err(|error| format!("cannot start collector capture worker: {error}"))?;
            Ok(Self {
                active: Some(Active::Process(Process {
                    #[cfg(test)]
                    id,
                    stop: stopped,
                    handle: Some(handle),
                    policy,
                })),
                completion: None,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (command, route, policy, files, dependency);
            Err("owned collector process capture requires Linux native gate supervision".into())
        }
    }
    pub fn poll(&mut self) -> Result<Option<Completion>, String> {
        if let Some(completion) = &self.completion {
            return Ok(Some(completion.clone()));
        }
        #[cfg(target_os = "linux")]
        if let Some(Active::Process(process)) = self.active.as_ref()
            && process
                .handle
                .as_ref()
                .is_none_or(|handle| handle.is_finished())
        {
            return Ok(Some(self.stop()));
        }
        Ok(None)
    }
    pub fn stop(&mut self) -> Completion {
        self.stop_before(Instant::now() + self.stop_budget())
    }
    pub fn request_stop(&self) {
        match self.active.as_ref() {
            #[cfg(target_os = "linux")]
            Some(Active::Process(process)) => process.stop.store(true, Ordering::Release),
            Some(Active::Snapshot(snapshot)) => snapshot.request_stop(),
            None => {}
        }
    }
    pub fn stop_budget(&self) -> Duration {
        match self.active.as_ref() {
            #[cfg(target_os = "linux")]
            Some(Active::Process(process)) => {
                process.policy.graceful.unwrap_or_default() + Duration::from_secs(8)
            }
            _ => Duration::from_secs(6),
        }
    }
    pub fn stop_before(&mut self, deadline: Instant) -> Completion {
        self.stop_before_with_owner(deadline, None)
    }
    pub(super) fn stop_before_with_owner(
        &mut self,
        deadline: Instant,
        owner: Option<Arc<crate::output_lock::OutputLock>>,
    ) -> Completion {
        if let Some(completion) = &self.completion {
            return completion.clone();
        }
        let completion = match self.active.take() {
            #[cfg(target_os = "linux")]
            Some(Active::Process(mut process)) => process.settle_before(deadline, owner),
            Some(Active::Snapshot(snapshot)) => {
                match snapshot.stop_before_with_owner(deadline, owner) {
                    Ok(()) => Completion {
                        exit: None,
                        guardian_cleanup_confirmed: None,
                        capture_complete: true,
                        interruption_requested: false,
                        interruption_acknowledged: false,
                        error: None,
                    },
                    Err(error) => Completion::failed(None, error),
                }
            }
            None => Completion::failed(
                None,
                "collector ownership already transferred; settlement unconfirmed",
            ),
        };
        self.completion = Some(completion.clone());
        completion
    }
    #[cfg(test)]
    pub fn completed(exit: ExitStatus) -> Self {
        Self::observed(Completion::test_completed(exit))
    }
    #[cfg(test)]
    pub fn observed(completion: Completion) -> Self {
        Self {
            active: None,
            completion: Some(completion),
        }
    }
    #[cfg(test)]
    pub fn id(&self) -> Option<u32> {
        match self.active.as_ref() {
            #[cfg(target_os = "linux")]
            Some(Active::Process(process)) => Some(process.id),
            _ => None,
        }
    }
}
impl Drop for Producer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(target_os = "linux")]
struct Process {
    #[cfg(test)]
    id: u32,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Completion>>,
    policy: Policy,
}
#[cfg(target_os = "linux")]
impl Process {
    fn settle_before(
        &mut self,
        deadline: Instant,
        owner: Option<Arc<crate::output_lock::OutputLock>>,
    ) -> Completion {
        self.stop.store(true, Ordering::Release);
        let Some(handle) = self.handle.take() else {
            return Completion::failed(None, "collector worker custody is unconfirmed");
        };
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !handle.is_finished() {
            super::retain_owner_until_worker_exit(handle, owner);
            let mut completion = Completion::failed(
                None,
                "collector settlement deadline exceeded; existing worker retains guardian and raw capture custody",
            );
            completion.guardian_cleanup_confirmed = Some(false);
            return completion;
        }
        handle.join().unwrap_or_else(|_| {
            Completion::failed(
                None,
                "collector capture worker panicked; capture completeness unconfirmed",
            )
        })
    }
}

#[cfg(target_os = "linux")]
fn capture(
    mut gate: crate::process_gate::ManagedGate,
    mut files: [File; 2],
    policy: Policy,
    stop: &AtomicBool,
    _dependency: Option<Arc<dyn Send + Sync>>,
    #[cfg(test)] fault: Option<Fault>,
) -> Completion {
    let mut exit = None;
    let mut cleanup_confirmed = false;
    let mut out = gate.take_stdout();
    let mut err = gate.take_stderr();
    let mut pipes_nonblocking = false;
    let mut interruption_requested = false;
    #[cfg(test)]
    let mut injected = false;
    // Gate, files and dependency live outside the unwind scope. A broken pump
    // cannot release a service while its producer's tree is still uncertain.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
        let out = out.as_mut().ok_or("missing collector stdout pipe")?;
        let err = err.as_mut().ok_or("missing collector stderr pipe")?;
        super::snapshot::nonblocking(out)
            .and_then(|()| super::snapshot::nonblocking(err))
            .map_err(|error| error.to_string())?;
        pipes_nonblocking = true;
        if stop.load(Ordering::Acquire) {
            return Err("collector cancelled before principal admission".into());
        }
        gate.resume().map_err(|error| error.to_string())?;
        let mut lengths = [0, 0];
        let mut eof = [false; 2];
        let mut grace = None;
        let mut stopped = false;
        let mut drain = None;
        loop {
            if drain.is_none() {
                let observed = gate.try_wait();
                exit = gate.principal_exit().map(|principal| principal.status());
                if observed.map_err(|error| error.to_string())?.is_some() {
                    cleanup_confirmed = true;
                    drain = Some(Instant::now() + Duration::from_secs(1));
                }
            }
            if stop.load(Ordering::Acquire) && !stopped && drain.is_none() {
                stopped = true;
                if let Some(duration) = policy.graceful {
                    let deadline = Instant::now() + duration;
                    if exit.is_none() {
                        gate.interrupt_principal(deadline).map_err(|error| {
                            format!("collector principal interruption failed: {error}")
                        })?;
                        interruption_requested = true;
                    }
                    grace = Some(deadline);
                } else {
                    return Err("collector was cancelled without a graceful flush contract; raw capture retained".into());
                }
            }
            if drain.is_none() && grace.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err("collector graceful flush deadline exceeded; forced cancellation cannot prove complete capture".into());
            }
            for (index, pipe) in [
                (0, &mut *out as &mut dyn Read),
                (1, &mut *err as &mut dyn Read),
            ] {
                pump(
                    pipe,
                    &mut files[index],
                    &mut lengths[index],
                    &mut eof[index],
                    policy.stream_bytes,
                    drain.or(grace),
                )?;
            }
            #[cfg(test)]
            if !injected && lengths.iter().any(|length| *length > 0)
                && let Some(fault) = &fault
            {
                injected = true;
                fault.reached.store(true, Ordering::Release);
                match fault.kind {
                    "read" => return Err("cannot read collector capture: injected offline read error".into()),
                    "panic" => panic!("injected offline pump panic"),
                    _ => {}
                }
            }
            if drain.is_some() && eof.iter().all(|eof| *eof) {
                for file in &mut files {
                    file.flush()
                        .map_err(|error| format!("cannot flush collector capture: {error}"))?;
                }
                if exit.is_none() {
                    return Err("collector principal exit is unknown despite guardian settlement; raw diagnostics retained".into());
                }
                return Ok(());
            }
            if drain.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err("collector pipe remained open after confirmed guardian quiescence; raw capture retained".into());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })).unwrap_or_else(|_| Err("collector capture pump panicked; raw capture retained".into()));
    match outcome {
        Ok(()) => Completion {
            exit,
            guardian_cleanup_confirmed: Some(cleanup_confirmed),
            capture_complete: true,
            interruption_requested,
            interruption_acknowledged: gate.interruption_acknowledged(),
            error: None,
        },
        Err(error) => {
            // Do not hand only the child to the generic reaper: capture files
            // and producer dependencies have the same proof-aware lifetime.
            // Parent settlement is bounded independently; this existing worker
            // conservatively retains custody on protocol uncertainty.
            #[cfg(test)]
            if let Some(fault) = &fault {
                fault.reached.store(true, Ordering::Release);
                while !fault.release.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            let _ = gate.request_cancel();
            while !cleanup_confirmed {
                if pipes_nonblocking {
                    // Failed capture is not repaired by a later prefix. Drain
                    // only to avoid blocking guardian diagnostics/teardown;
                    // retained raw files remain explicitly incomplete.
                    for pipe in [
                        out.as_mut().map(|pipe| pipe as &mut dyn Read),
                        err.as_mut().map(|pipe| pipe as &mut dyn Read),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        let mut discard = [0; 4096];
                        for _ in 0..16 {
                            if !matches!(pipe.read(&mut discard), Ok(count) if count > 0) {
                                break;
                            }
                        }
                    }
                }
                if matches!(gate.try_wait(), Ok(Some(_))) {
                    cleanup_confirmed = true;
                } else {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            exit = gate
                .principal_exit()
                .map(|principal| principal.status())
                .or(exit);
            let mut completion = Completion::failed(exit, error);
            completion.guardian_cleanup_confirmed = Some(cleanup_confirmed);
            completion.interruption_requested = interruption_requested;
            completion.interruption_acknowledged = gate.interruption_acknowledged();
            completion
        }
    }
}

#[cfg(target_os = "linux")]
fn pump(
    reader: &mut dyn Read,
    writer: &mut dyn Write,
    length: &mut u64,
    eof: &mut bool,
    limit: u64,
    deadline: Option<Instant>,
) -> Result<(), String> {
    for _ in 0..16 {
        if *eof {
            break;
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        let mut buffer = [0; 4096];
        match reader.read(&mut buffer) {
            Ok(0) => *eof = true,
            Ok(count) => {
                *length = length
                    .checked_add(count as u64)
                    .filter(|length| *length <= limit)
                    .ok_or("collector capture exceeds adapter byte limit")?;
                writer
                    .write_all(&buffer[..count])
                    .map_err(|error| format!("cannot write collector capture: {error}"))?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(format!("cannot read collector capture: {error}")),
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicUsize;

    struct Fixture {
        path: PathBuf,
        release: Arc<AtomicBool>,
        sentinel: std::process::Child,
    }
    impl Fixture {
        fn new() -> Self {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/collector-producer-fixtures")
                .join(crate::workloads::TaskIdentity::local("producer", None).attempt_id);
            std::fs::create_dir_all(&path).unwrap();
            Self {
                path,
                release: Arc::new(AtomicBool::new(false)),
                sentinel: std::process::Command::new("/bin/sleep")
                    .arg("30")
                    .spawn()
                    .unwrap(),
            }
        }
        fn spawn(
            &self,
            script: &str,
            dependency: Option<Arc<dyn Send + Sync>>,
            full: bool,
        ) -> Producer {
            let mut launch = crate::launch::ProcessLaunch::new("python3");
            launch.arg("-I").arg("-S").arg("-u").arg("-c").arg(script);
            let (mut command, route) = super::command(&mut launch).unwrap();
            Producer::spawn_with_dependency(
                &mut command,
                route,
                Policy {
                    stream_bytes: 1024 * 1024,
                    graceful: Some(Duration::from_secs(2)),
                },
                [
                    if full {
                        std::fs::OpenOptions::new()
                            .write(true)
                            .open("/dev/full")
                            .unwrap()
                    } else {
                        File::create(self.path.join("stdout")).unwrap()
                    },
                    File::create(self.path.join("stderr")).unwrap(),
                ],
                dependency,
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.release.store(true, Ordering::Release);
            let _ = self.sentinel.kill();
            let _ = self.sentinel.wait();
            // Capture evidence is intentionally retained for failed assertions.
        }
    }
    fn until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "owned producer fixture timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    struct Dependency(Arc<AtomicUsize>);
    impl Drop for Dependency {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn graceful_flush_drains_both_pipes_before_complete_capture() {
        let fixture = Fixture::new();
        let script = "import signal,sys\ndef done(*_):\n sys.stdout.write('O'*196608);sys.stdout.flush()\n sys.stderr.write('E'*196608);sys.stderr.flush()\n sys.exit(7)\nsignal.signal(signal.SIGINT,done)\nprint('READY',flush=True)\nsignal.pause()\n";
        let mut producer = fixture.spawn(script, None, false);
        until(|| {
            std::fs::read(fixture.path.join("stdout"))
                .is_ok_and(|data| data.starts_with(b"READY\n"))
        });
        let completion = producer.stop();
        assert_eq!(completion.exit.and_then(|status| status.code()), Some(7));
        assert_eq!(completion.guardian_cleanup_confirmed, Some(true));
        assert!(completion.capture_complete, "{:?}", completion.error);
        assert_eq!(
            std::fs::read(fixture.path.join("stdout")).unwrap().len(),
            196614
        );
        assert_eq!(
            std::fs::read(fixture.path.join("stderr")).unwrap().len(),
            196608
        );
        assert!(producer.stop().capture_complete);
    }

    #[test]
    fn failed_pump_retains_dependency_beyond_parent_wait_until_guardian_quiescence() {
        for kind in ["read", "write", "panic"] {
            let mut fixture = Fixture::new();
            let reached = Arc::new(AtomicBool::new(false));
            FAULT.with(|slot| {
                *slot.borrow_mut() = Some(Fault {
                    kind,
                    reached: Arc::clone(&reached),
                    release: Arc::clone(&fixture.release),
                })
            });
            let drops = Arc::new(AtomicUsize::new(0));
            let dependency = Arc::new(Dependency(Arc::clone(&drops)));
            let mut producer = fixture.spawn(
                "import signal\nprint('READY',flush=True)\nsignal.pause()\n",
                Some(dependency),
                kind == "write",
            );
            until(|| reached.load(Ordering::Acquire));
            let completion = producer.stop_before(Instant::now() + Duration::from_millis(30));
            assert!(!completion.capture_complete);
            assert_eq!(completion.guardian_cleanup_confirmed, Some(false));
            assert_eq!(
                drops.load(Ordering::Acquire),
                0,
                "{kind}: dependency released before proof"
            );
            assert!(fixture.sentinel.try_wait().unwrap().is_none());
            fixture.release.store(true, Ordering::Release);
            until(|| drops.load(Ordering::Acquire) == 1);
            // Parent's unknown result remains immutable after late settlement.
            assert_eq!(producer.stop().guardian_cleanup_confirmed, Some(false));
            assert!(fixture.sentinel.try_wait().unwrap().is_none());
        }
    }

    #[test]
    fn pump_read_errors_and_write_zero_cannot_validate_a_prefix() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("synthetic read fault"))
            }
        }
        struct Zero;
        impl Write for Zero {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Ok(0)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut count = 0;
        let mut eof = false;
        assert!(
            pump(
                &mut Broken,
                &mut Vec::new(),
                &mut count,
                &mut eof,
                4096,
                None
            )
            .unwrap_err()
            .contains("synthetic read fault")
        );
        assert!(
            pump(
                &mut &b"prefix"[..],
                &mut Zero,
                &mut count,
                &mut eof,
                4096,
                None
            )
            .unwrap_err()
            .contains("cannot write collector capture")
        );
        assert!(!eof);
    }

    #[test]
    fn missing_executable_and_invalid_route_fail_before_principal_admission() {
        let mut fixture = Fixture::new();
        let missing = fixture.path.join("missing-tool");
        let mut launch = crate::launch::ProcessLaunch::new(&missing);
        let error = super::command(&mut launch).err().unwrap();
        assert!(error.contains("missing or not executable"));
        assert!(error.contains(missing.to_str().unwrap()));
        assert!(std::fs::read_dir(&fixture.path).unwrap().next().is_none());

        let mut launch = crate::launch::ProcessLaunch::new("/bin/sh");
        launch.arg("-c").arg("exit 99");
        let (_, route) = super::command(&mut launch).unwrap();
        let mut replaced = Command::new("/bin/false");
        let drops = Arc::new(AtomicUsize::new(0));
        let result = Producer::spawn_with_dependency(
            &mut replaced,
            route,
            Policy {
                stream_bytes: 1024,
                graceful: None,
            },
            [
                File::create(fixture.path.join("stdout")).unwrap(),
                File::create(fixture.path.join("stderr")).unwrap(),
            ],
            Some(Arc::new(Dependency(Arc::clone(&drops)))),
        );
        assert!(result.err().unwrap().contains("no longer matches"));
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(fixture.sentinel.try_wait().unwrap().is_none());
    }

    #[test]
    fn executable_removed_after_precheck_is_actual_failed_capture_not_unavailable_success() {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = Fixture::new();
        let executable = fixture.path.join("producer");
        std::fs::write(&executable, "#!/bin/sh\nprintf 'must not run'\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut launch = crate::launch::ProcessLaunch::new(&executable);
        let (mut command, route) = super::command(&mut launch).unwrap();
        std::fs::remove_file(&executable).unwrap();
        let mut producer = Producer::spawn_with_dependency(
            &mut command,
            route,
            Policy {
                stream_bytes: 1024,
                graceful: Some(Duration::from_secs(1)),
            },
            [
                File::create(fixture.path.join("stdout")).unwrap(),
                File::create(fixture.path.join("stderr")).unwrap(),
            ],
            None,
        )
        .unwrap();
        let mut completion = None;
        until(|| {
            completion = producer.poll().unwrap();
            completion.is_some()
        });
        let completion = completion.unwrap();
        assert!(!completion.capture_complete);
        assert!(
            completion.exit.is_none(),
            "no principal exit may be invented for failed exec"
        );
        assert_eq!(completion.guardian_cleanup_confirmed, Some(true));
        assert!(completion.error.is_some());
        let diagnostic = std::fs::read_to_string(fixture.path.join("stderr")).unwrap();
        assert!(diagnostic.contains("No such file"), "{diagnostic}");
        assert!(
            std::fs::read(fixture.path.join("stdout"))
                .unwrap()
                .is_empty()
        );
        assert!(fixture.sentinel.try_wait().unwrap().is_none());
    }

    #[test]
    fn real_grace_expiry_retains_dependency_after_parent_deadline_without_fault_hooks() {
        let mut fixture = Fixture::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let mut producer = fixture.spawn(
            "import signal\nsignal.signal(signal.SIGINT,signal.SIG_IGN)\nprint('READY',flush=True)\nsignal.pause()\n",
            Some(Arc::new(Dependency(Arc::clone(&drops)))), false);
        until(|| {
            std::fs::read(fixture.path.join("stdout"))
                .is_ok_and(|data| data.starts_with(b"READY\n"))
        });
        let started = Instant::now();
        let completion = producer.stop_before(started + Duration::from_millis(30));
        assert!(started.elapsed() < Duration::from_millis(600));
        assert!(!completion.capture_complete);
        assert_eq!(completion.guardian_cleanup_confirmed, Some(false));
        assert_eq!(drops.load(Ordering::Acquire), 0);
        assert!(fixture.sentinel.try_wait().unwrap().is_none());
        until(|| drops.load(Ordering::Acquire) == 1);
        assert!(
            started.elapsed() >= Duration::from_secs(1),
            "dependency released before real grace expired"
        );
        assert_eq!(producer.stop().guardian_cleanup_confirmed, Some(false));
        assert!(
            std::fs::read(fixture.path.join("stdout"))
                .unwrap()
                .starts_with(b"READY\n")
        );
        assert!(fixture.sentinel.try_wait().unwrap().is_none());
    }
}
