//! Actual native admission with entirely inert, offline fixture programs.
#![allow(unsafe_code)] // Observe pinned fixture identities using pidfd, never signal.
use super::*;
use std::fs;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Condvar, Mutex, mpsc};

fn root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/installer-process-core")
        .join(crate::tools::runtime::unique_id());
    fs::create_dir_all(&root).unwrap();
    fs::canonicalize(root).unwrap()
}
fn context(root: &Path) -> OperationContext {
    let config = ProjectConfig::default();
    OperationContext::new(root, &config).unwrap()
}
fn shell(context: &OperationContext, script: &str) -> PreparedStep {
    let mut step =
        PreparedStep::new(context, "/bin/sh", Purpose::Probe, "offline fixture").unwrap();
    step.command_mut()
        .args(["-c", script])
        .current_dir(&context.root)
        .stdin(std::process::Stdio::null());
    step
}
fn wait_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "missing handshake {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn abrupt_owner_fixture() {
    let Some(root) = std::env::var_os("ASB_INSTALL_DEATH_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let context = context(&root).with_destination(&root.join("tool")).unwrap();
    let _ = shell(&context, "printf started > marker; exec sleep 60").run(ProducerLease::default());
}

#[test]
fn killed_installer_leaves_guardian_owning_destination_until_cleanup() {
    struct Cleanup {
        parent: std::process::Child,
        guardian: Option<i32>,
        stopped_children: Vec<i32>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.parent.kill();
            let _ = self.parent.wait();
            for pid in &self.stopped_children {
                unsafe {
                    libc::kill(*pid, libc::SIGCONT);
                }
            }
            if let Some(pid) = self.guardian {
                unsafe {
                    libc::kill(pid, libc::SIGCONT);
                }
            }
        }
    }
    let root = root();
    let parent = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "install::process::tests::abrupt_owner_fixture",
            "--nocapture",
        ])
        .env("ASB_INSTALL_DEATH_FIXTURE", &root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut cleanup = Cleanup {
        parent,
        guardian: None,
        stopped_children: Vec::new(),
    };
    wait_file(&root.join("marker"));
    let tasks = PathBuf::from(format!("/proc/{}/task", cleanup.parent.id()));
    let mut children = Vec::new();
    for task in fs::read_dir(tasks).unwrap().flatten() {
        if let Ok(value) = fs::read_to_string(task.path().join("children")) {
            children.extend(
                value
                    .split_whitespace()
                    .filter_map(|value| value.parse::<i32>().ok()),
            );
        }
    }
    assert_eq!(
        children.len(),
        1,
        "fixture must have exactly one owned guardian"
    );
    let guardian = children[0];
    assert_eq!(unsafe { libc::kill(guardian, libc::SIGSTOP) }, 0);
    cleanup.guardian = Some(guardian);
    let guardian_children =
        fs::read_to_string(format!("/proc/{guardian}/task/{guardian}/children")).unwrap();
    for pid in guardian_children
        .split_whitespace()
        .filter_map(|value| value.parse::<i32>().ok())
    {
        if unsafe { libc::kill(pid, libc::SIGSTOP) } == 0 {
            cleanup.stopped_children.push(pid);
        }
    }
    assert!(
        !cleanup.stopped_children.is_empty(),
        "fixture watchdog and target must be present"
    );
    cleanup.parent.kill().unwrap();
    cleanup.parent.wait().unwrap();
    assert!(
        crate::install::destination_lock(&root.join("tool")).is_err(),
        "guardian must retain lock after installer death"
    );
    for pid in cleanup.stopped_children.drain(..) {
        unsafe {
            libc::kill(pid, libc::SIGCONT);
        }
    }
    assert_eq!(unsafe { libc::kill(guardian, libc::SIGCONT) }, 0);
    cleanup.guardian = None;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(owner) = crate::install::destination_lock(&root.join("tool")) {
            drop(owner);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "guardian never released ownership after cleanup"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn purpose_budgets_share_the_frozen_operation_and_render_only_at_boundary() {
    let root = root();
    let mut config = ProjectConfig::default();
    config.installation.operation_timeout_seconds = 2;
    let context = OperationContext::new(&root, &config).unwrap();
    config.installation.operation_timeout_seconds = 100;
    for purpose in [
        Purpose::Probe,
        Purpose::Version,
        Purpose::Download,
        Purpose::Extract,
        Purpose::Configure,
        Purpose::Package,
        Purpose::Build,
    ] {
        let (end, key) = context.step_deadline(purpose).unwrap();
        assert_eq!(end, context.deadline);
        assert_eq!(key, "operation_timeout_seconds");
    }
    let shorter = OperationContext::new(&root, &config).unwrap();
    assert_eq!(
        shorter.step_deadline(Purpose::Version).unwrap().1,
        "version_timeout_seconds"
    );
    context.cancel();
    let failure = match shell(&context, "exit 0").run(ProducerLease::default()) {
        Err(failure) => failure,
        Ok(_) => panic!("cancelled operation admitted a target"),
    };
    assert!(matches!(failure, InstallFailure::Abort(_)));
    assert!(failure.into_public().to_string().contains("Cancelled"));
}

#[test]
fn actual_prepared_command_preserves_configuration_and_private_logs() {
    let root = root();
    let context = context(&root);
    let mut launch = crate::launch::ProcessLaunch::new("/bin/sh");
    launch.arg0("intentional-installer-name");
    let mut step =
        PreparedStep::from_launch(&context, &launch, Purpose::Probe, "configuration fixture")
            .unwrap();
    step.command_mut()
        .env_clear()
        .env("EXPLICIT", "kept")
        .current_dir(&root)
        .args([
            "-c",
            "printf '%s|%s|%s' \"$0\" \"$EXPLICIT\" \"$PWD\"; printf 'private-stderr' >&2",
        ])
        .stdin(std::process::Stdio::null());
    step.capture(Capture::inspection());
    let result = step.run(ProducerLease::default()).unwrap();
    assert!(result.status().success());
    assert_eq!(
        String::from_utf8(result.stdout().to_vec()).unwrap(),
        format!("intentional-installer-name|kept|{}", root.display())
    );
    assert_eq!(result.stderr(), b"private-stderr");
    let path = result.log.path.unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(result.log.accepted > 0);
    assert!(result.log.retained <= result.log.accepted);
    assert_eq!(result.log.discarded, 0);
    assert!(result.log.fault.is_none());
    let _settled_diagnostic = result.log.settled;
}

#[test]
fn sensitive_prepared_command_does_not_create_a_diagnostic_log() {
    let root = root();
    let context = context(&root);
    let mut step = shell(
        &context,
        "printf sensitive-output; printf sensitive-error >&2",
    );
    step.suppress_diagnostics();
    let result = step.run(ProducerLease::default()).unwrap();
    assert!(result.status().success());
    assert!(result.stdout().is_empty());
    assert!(result.stderr().is_empty());
    assert!(result.log.path.is_none());
    assert_eq!(result.log.accepted, 0);
    assert_eq!(result.log.retained, 0);
    assert!(result.log.settled);
}

#[test]
fn actual_inspection_overflow_aborts_without_accepting_a_prefix() {
    let root = root();
    let context = context(&root);
    let mut step = shell(&context, "printf 'abcdefghijk'");
    step.capture(Capture::Inspect {
        stream_bytes: 4,
        total_bytes: 8,
    });
    match step.run(ProducerLease::default()) {
        Err(InstallFailure::Abort(abort)) => {
            assert_eq!(abort.cause, AbortCause::CaptureOverflow);
            assert!(abort.cleanup_confirmed);
            assert!(!abort.capture_complete);
        }
        _ => panic!("overflow was accepted or erased into an ordinary candidate failure"),
    }
}

struct HeldWriter {
    file: fs::File,
    reached: Arc<AtomicBool>,
    release: Arc<(Mutex<bool>, Condvar)>,
}
impl Write for HeldWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.reached.store(true, Ordering::Release);
        let (lock, wake) = &*self.release;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = wake.wait(released).unwrap();
        }
        self.file.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
struct Release(Arc<(Mutex<bool>, Condvar)>);
impl Drop for Release {
    fn drop(&mut self) {
        let (lock, wake) = &*self.0;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
}
struct Dependency(Arc<AtomicBool>, ProducerState);
impl ProducerOwner for Dependency {
    fn producer_state(&self) -> &ProducerState {
        &self.1
    }
    fn producer_started(&self) {}
}
impl Drop for Dependency {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn pin(pid: u32) -> fs::File {
    // SAFETY: pin a fixture PID only for poll observation, never signalling.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    assert!(
        fd >= 0,
        "cannot observe owned fixture: {}",
        io::Error::last_os_error()
    );
    unsafe { fs::File::from_raw_fd(fd) }
}
fn exited(identity: &fs::File) -> bool {
    let mut descriptor = libc::pollfd {
        fd: identity.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll only a live pinned descriptor, with no blocking timeout.
    (unsafe { libc::poll(&mut descriptor, 1, 0) }) == 1 && descriptor.revents & libc::POLLIN != 0
}

struct Sentinel(crate::process_gate::ManagedGate);
impl Sentinel {
    fn start() -> Self {
        let (mut command, route) =
            crate::launch::ProcessLaunch::new("/bin/sleep").native(Some(&runtime().unwrap()));
        command
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut gate =
            crate::process_gate::Admission::new(route.validate_gate(&mut command).unwrap())
                .unwrap()
                .spawn()
                .unwrap();
        gate.resume().unwrap();
        Self(gate)
    }
}
impl Drop for Sentinel {
    fn drop(&mut self) {
        let _ = self.0.stop();
    }
}

#[test]
fn blocked_log_writer_cannot_hold_cancellation_or_producer_tree_custody() {
    let root = root();
    let context = context(&root);
    let mut sentinel = Sentinel::start();
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let release_on_failure = Release(Arc::clone(&release));
    let reached = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let dependency = Arc::new(Dependency(Arc::clone(&dropped), ProducerState::default()));
    let lease = ProducerLease::new(dependency, &root);
    let mut step = shell(
        &context,
        "printf $$ > principal.pid; sleep 30 & echo $! > descendant.pid; printf 'inert-private-secret'; wait",
    );
    let held = Arc::clone(&reached);
    let writer_release = Arc::clone(&release);
    step.log_writer = Some(Box::new(move |file| {
        Box::new(HeldWriter {
            file,
            reached: held,
            release: writer_release,
        })
    }));
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        send.send(step.run(lease)).unwrap();
    });
    wait_file(&root.join("principal.pid"));
    wait_file(&root.join("descendant.pid"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !reached.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    // The writer handshake happens after both complete PID writes, avoiding
    // confusing an existing but not-yet-written marker with process identity.
    let principal = pin(fs::read_to_string(root.join("principal.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap());
    let descendant = pin(fs::read_to_string(root.join("descendant.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap());
    assert!(!dropped.load(Ordering::Acquire));
    context.cancel();
    let result = receive
        .recv_timeout(Duration::from_secs(8))
        .expect("cancellation waited for blocked log writer");
    let abort = match result {
        Err(InstallFailure::Abort(abort)) => abort,
        _ => panic!("cancellation was erased"),
    };
    assert!(abort.cleanup_confirmed, "no matching guardian quiescence");
    assert!(exited(&principal));
    assert!(exited(&descendant));
    assert!(
        sentinel.0.try_wait().unwrap().is_none(),
        "unrelated sentinel was stopped"
    );
    assert!(
        !*release.0.lock().unwrap(),
        "writer was released before custody proof"
    );
    // The writer is released only AFTER actual pinned child/descendant exit and
    // the matching native guardian quiescence result were established.
    drop(release_on_failure);
}

#[test]
fn actual_step_deadline_cancels_a_stuck_helper_without_a_reset_budget() {
    let root = root();
    let mut context = context(&root);
    context.deadline = Instant::now() + Duration::from_millis(100);
    let started = Instant::now();
    let failure =
        match shell(&context, "printf started > ready; sleep 30").run(ProducerLease::default()) {
            Err(InstallFailure::Abort(abort)) => abort,
            _ => panic!("deadline became a successful step or an ordinary candidate miss"),
        };
    assert_eq!(failure.cause, AbortCause::Deadline);
    assert!(failure.cleanup_confirmed);
    assert!(started.elapsed() < Duration::from_secs(7));
}

#[test]
fn an_exited_principal_cannot_leave_a_writer_mutating_staging_after_settlement() {
    let root = root();
    let context = context(&root);
    fs::write(root.join("stage"), "initial").unwrap();
    let step = shell(
        &context,
        "(while ! test -f release; do sleep .01; done; printf wrong > stage) & exit 0",
    );
    let result = step.run(ProducerLease::default()).unwrap();
    assert!(result.status().success());
    fs::write(root.join("release"), "release-after-settlement").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(fs::read_to_string(root.join("stage")).unwrap(), "initial");
}

struct ObserveRelease(Arc<AtomicBool>);
impl Drop for ObserveRelease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[test]
fn parent_wait_expiry_retains_the_real_dependency_until_matching_quiescence() {
    let root = root();
    let context = context(&root);
    let mut sentinel = Sentinel::start();
    let held = Arc::new(AtomicBool::new(true));
    let release_on_failure = ObserveRelease(Arc::clone(&held));
    let dropped = Arc::new(AtomicBool::new(false));
    let lease = ProducerLease::new(
        Arc::new(Dependency(Arc::clone(&dropped), ProducerState::default())),
        &root,
    );
    let mut step = shell(&context, "printf ready > ready; sleep 30");
    step.hold_observation = Some(Arc::clone(&held));
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = send.send(step.run(lease));
    });
    wait_file(&root.join("ready"));
    context.cancel();
    let failure = match receive.recv_timeout(Duration::from_secs(8)).unwrap() {
        Err(InstallFailure::Abort(abort)) => abort,
        _ => panic!("parent wait was converted into settled success"),
    };
    assert!(!failure.cleanup_confirmed);
    assert!(
        !dropped.load(Ordering::Acquire),
        "unknown cleanup released the actual producer dependency"
    );
    assert!(sentinel.0.try_wait().unwrap().is_none());
    drop(release_on_failure);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !dropped.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "matching settlement did not release retained dependency"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(sentinel.0.try_wait().unwrap().is_none());
}

struct ContinueRelease(Arc<AtomicBool>);
impl Drop for ContinueRelease {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn paused_admission_must_not_resume_after_abort(expire: bool) {
    let root = root();
    let mut context = context(&root)
        .with_destination(&root.join("owned-tool"))
        .unwrap();
    if expire {
        context.deadline = Instant::now() + Duration::from_millis(500);
    }
    let mut sentinel = Sentinel::start();
    let reached = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let settled = Arc::new(AtomicBool::new(false));
    let release_guard = ContinueRelease(Arc::clone(&released));
    let mut step = shell(&context, "printf forbidden > client-started; sleep 30");
    step.before_continue = Some(native::AdmissionPause {
        reached: Arc::clone(&reached),
        release: Arc::clone(&released),
        settled: Arc::clone(&settled),
        client_marker: root.join("client-started"),
    });
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = send.send(step.run(ProducerLease::default()));
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !reached.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "stopped readiness hook not reached"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    if !expire {
        context.cancel();
    }
    let result = receive.recv_timeout(Duration::from_secs(8)).unwrap();
    assert!(matches!(result, Err(InstallFailure::Abort(_))));
    drop(context);
    assert!(
        crate::install::destination_lock(&root.join("owned-tool")).is_err(),
        "uncertain worker must retain installation ownership"
    );
    assert!(!root.join("client-started").exists());
    drop(release_guard);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !settled.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "no actual matching guardian quiescence after release"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(owner) = crate::install::destination_lock(&root.join("owned-tool")) {
            drop(owner);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "settled worker retained installation ownership"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(sentinel.0.try_wait().unwrap().is_none());
    assert!(
        !root.join("client-started").exists(),
        "core resumed the client AFTER caller Abort; {}",
        root.display()
    );
}

#[test]
fn core_review_cancelled_stopped_guardian_must_not_resume_after_parent_abort() {
    paused_admission_must_not_resume_after_abort(false);
}
#[test]
fn core_review_expired_stopped_guardian_must_not_resume_after_parent_abort() {
    paused_admission_must_not_resume_after_abort(true);
}

#[test]
fn core_review_step_preparation_uses_frozen_config_not_a_fresh_invalid_file() {
    let root = root();
    fs::create_dir(root.join(".asb")).unwrap();
    let path = root.join(".asb/project.toml");
    let config = ProjectConfig::default();
    fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let (_, loaded) = crate::project::load(&root).unwrap();
    let context = OperationContext::new(&root, &loaded).unwrap();
    let mut invalid = config;
    invalid.installation.probe_timeout_seconds = 0;
    fs::write(&path, toml::to_string(&invalid).unwrap()).unwrap();
    assert!(
        crate::project::load(&root).is_err(),
        "replacement fixture was not invalid"
    );
    assert!(
        PreparedStep::new(
            &context,
            "/bin/true",
            Purpose::Probe,
            "frozen config fixture"
        )
        .is_ok(),
        "preparation reloaded a configuration that should be frozen for this operation"
    );
}

#[test]
fn explicit_operation_proxy_snapshot_is_not_replaced_by_foreign_network_scope() {
    let foreign = root();
    fs::create_dir(foreign.join(".asb")).unwrap();
    let mut invalid = ProjectConfig::default();
    invalid.installation.download_timeout_seconds = 0;
    fs::write(
        foreign.join(".asb/project.toml"),
        toml::to_string(&invalid).unwrap(),
    )
    .unwrap();
    let root = root();
    let config = ProjectConfig {
        proxy: crate::project::NetworkProxy {
            http: Some("http://127.0.0.1:3128".into()),
            no_proxy: Some("frozen-operation.invalid".into()),
            ..Default::default()
        },
        ..ProjectConfig::default()
    };
    let context = OperationContext::new(&root, &config).unwrap();
    let mut expected = std::process::Command::new("/bin/true");
    config
        .proxy
        .apply(&mut expected, &|name| std::env::var(name).ok());
    let _foreign_scope = crate::project::network_project(&foreign);
    let step = PreparedStep::new(
        &context,
        "/bin/true",
        Purpose::Probe,
        "explicit project fixture",
    )
    .unwrap();
    let environments = |command: &std::process::Command| {
        command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(std::ffi::OsStr::to_owned)))
            .collect::<Vec<_>>()
    };
    assert!(
        environments(&step.command) == environments(&expected),
        "the explicit operation proxy snapshot was not used; values omitted to protect environment credentials"
    );
    assert!(
        crate::project::load(&foreign)
            .unwrap_err()
            .to_string()
            .contains("installation.download_timeout_seconds")
    );
    assert_eq!(context.root, root);
}

#[test]
fn actual_staging_rejects_second_lane_and_stale_permission_then_publishes_only_matching_proof() {
    let root = root();
    let context = context(&root);
    let destination = root.join("installed");
    fs::write(&destination, "old-installed").unwrap();
    let staging = Arc::new(crate::install::source::Staging::new(&destination).unwrap());
    let source_path = staging.producer_path().to_owned();
    struct ReleaseOnUnwind(PathBuf);
    impl Drop for ReleaseOnUnwind {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "release");
        }
    }
    let release = ReleaseOnUnwind(root.join("release"));
    let mut lane = staging.producer_scope(&context);
    let stale = staging.producer_scope(&context).finish().unwrap();
    let second_lane = staging.producer_scope(&context);
    let mut step = lane
        .prepare("/bin/sh", Purpose::Probe, "actual staging producer")
        .unwrap();
    step.command_mut().env_clear().env("PATH", "/usr/bin:/bin").current_dir(&root)
        .args(["-c", "printf complete > \"$1\"; printf started > ready; while ! test -f release; do sleep .01; done; printf done; printf diagnostic >&2", "fixture"])
        .arg(&source_path).stdin(std::process::Stdio::null());
    step.capture(Capture::inspection());
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = lane.run(step);
        let _ = send.send((lane, result));
    });
    wait_file(&root.join("ready"));
    assert!(
        staging.accept_producer_permission(stale).is_err(),
        "stale never-admitted token authorized a live producer"
    );
    assert!(
        matches!(second_lane.finish(), Err(InstallFailure::Abort(_))),
        "second lane minted proof while the actual writer was active"
    );
    assert_eq!(fs::read_to_string(&destination).unwrap(), "old-installed");
    assert!(source_path.exists());
    fs::write(&release.0, "release").unwrap();
    let (lane, result) = receive.recv_timeout(Duration::from_secs(5)).unwrap();
    let result = result.unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout, b"done");
    assert_eq!(result.stderr, b"diagnostic");
    assert!(result.log.path.is_some());
    let permission = lane.finish().unwrap();
    let stage = Arc::try_unwrap(staging)
        .ok()
        .expect("actual stage owner remains held after quiescence");
    stage
        .publish_with_permission_for_test(permission, &destination)
        .unwrap();
    assert_eq!(fs::read_to_string(destination).unwrap(), "complete");
    assert!(!source_path.exists());
}

#[test]
fn actual_staging_alternate_constructor_cannot_restore_old_permission_during_admission() {
    let root = root();
    let context = context(&root);
    let destination = root.join("installed");
    fs::write(&destination, "old-installed").unwrap();
    let staging = Arc::new(crate::install::source::Staging::new(&destination).unwrap());
    let old_permission = staging.producer_scope(&context).finish().unwrap();
    let alternate = ProducerLease::new(Arc::clone(&staging), &root);
    let mut lane = ProcessScope::new(&context, alternate);
    let mut sentinel = Sentinel::start();
    let mut step = lane
        .prepare(
            "/bin/sh",
            Purpose::Probe,
            "same actual owner alternate lease",
        )
        .unwrap();
    step.command_mut().env_clear().env("PATH", "/usr/bin:/bin").current_dir(&root)
        .args(["-c", "printf partial > \"$1\"; printf ready > ready; while ! test -f release; do sleep .01; done", "fixture"])
        .arg(staging.producer_path()).stdin(std::process::Stdio::null());
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = lane.run(step);
        let _ = send.send(result.is_ok());
    });
    struct ReleaseFile(PathBuf);
    impl Drop for ReleaseFile {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "release");
        }
    }
    let release = ReleaseFile(root.join("release"));
    wait_file(&root.join("ready"));
    let denied = staging.accept_producer_permission(old_permission).is_err();
    fs::write(&release.0, "release").unwrap();
    assert!(
        receive.recv_timeout(Duration::from_secs(5)).unwrap(),
        "actual producer did not settle"
    );
    let sentinel_alive = sentinel.0.try_wait().unwrap().is_none();
    sentinel.0.stop().unwrap();
    assert!(sentinel_alive);
    assert_eq!(fs::read_to_string(destination).unwrap(), "old-installed");
    assert!(staging.producer_path().exists());
    fs::write(
        root.join("alternate-owner-cleanup-confirmed"),
        "actual helper settled; sentinel independently stopped",
    )
    .unwrap();
    assert!(
        denied,
        "alternate constructor left old actual-owner permission valid during admission"
    );
}

#[test]
fn actual_staging_rejects_other_owner_proof_without_publishing_or_cleaning_its_bytes() {
    let root = root();
    let context = context(&root);
    let destination = root.join("installed");
    fs::write(&destination, "old-installed").unwrap();
    let stage = Arc::new(crate::install::source::Staging::new(&destination).unwrap());
    let source_path = stage.producer_path().to_owned();
    let other = Arc::new(crate::install::source::Staging::new(&root.join("other")).unwrap());
    let mut other_lane = other.producer_scope(&context);
    let mut step = other_lane
        .prepare("/bin/true", Purpose::Probe, "different stage producer")
        .unwrap();
    step.command_mut().stdin(std::process::Stdio::null());
    assert!(other_lane.run(step).unwrap().status.success());
    let unrelated = other_lane.finish().unwrap();
    let stage = Arc::try_unwrap(stage)
        .ok()
        .expect("target has no admitted producer");
    assert!(
        stage
            .publish_with_permission_for_test(unrelated, &destination)
            .is_err()
    );
    assert_eq!(fs::read_to_string(destination).unwrap(), "old-installed");
    assert!(
        source_path.exists(),
        "wrong-owner denial destructively cleaned the target stage"
    );
}

#[test]
fn matching_permission_is_consumed_once_and_a_later_attempt_invalidates_old_receipts() {
    let root = root();
    let context = context(&root);
    let lease = ProducerLease::default();
    let state = lease.state.clone();
    let first = ProcessScope::new(&context, lease.clone()).finish().unwrap();
    let duplicate = ProcessScope::new(&context, lease.clone()).finish().unwrap();
    first.consume_for(&state).unwrap();
    assert!(matches!(
        duplicate.consume_for(&state),
        Err(InstallFailure::Abort(_))
    ));
    let step = shell(&context, "printf forbidden > must-not-execute");
    assert!(matches!(step.run(lease), Err(InstallFailure::Abort(_))));
    assert!(!root.join("must-not-execute").exists());
}

#[test]
fn unknown_cleanup_keeps_actual_staging_owner_then_retains_raw_after_late_quiescence() {
    let root = root();
    let context = context(&root);
    let mut sentinel = Sentinel::start();
    let destination = root.join("installed");
    fs::write(&destination, "old-installed").unwrap();
    let staging = Arc::new(crate::install::source::Staging::new(&destination).unwrap());
    let weak = Arc::downgrade(&staging);
    let source_path = staging.producer_path().to_owned();
    let mut lane = staging.producer_scope(&context);
    let held = Arc::new(AtomicBool::new(true));
    let release = ObserveRelease(Arc::clone(&held));
    let mut step = lane
        .prepare(
            "/bin/sh",
            Purpose::Probe,
            "retained actual staging producer",
        )
        .unwrap();
    step.command_mut()
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .current_dir(&root)
        .args([
            "-c",
            "printf partial > \"$1\"; printf ready > ready; sleep 30",
            "fixture",
        ])
        .arg(&source_path)
        .stdin(std::process::Stdio::null());
    step.hold_observation = Some(Arc::clone(&held));
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = lane.run(step);
        let _ = send.send(matches!(result, Err(InstallFailure::Abort(_))));
        drop(lane);
    });
    wait_file(&root.join("ready"));
    context.cancel();
    assert!(receive.recv_timeout(Duration::from_secs(8)).unwrap());
    drop(staging); // Worker, not a test-local path or guard, must keep it alive.
    assert!(
        weak.upgrade().is_some(),
        "actual owner vanished before matching quiescence"
    );
    assert!(source_path.exists());
    assert!(sentinel.0.try_wait().unwrap().is_none());
    drop(release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while weak.upgrade().is_some() {
        assert!(
            Instant::now() < deadline,
            "actual settled worker did not release its owner"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        source_path.exists(),
        "late owner Drop destroyed evidence after an uncertain return"
    );
    assert_eq!(fs::read_to_string(destination).unwrap(), "old-installed");
    assert!(sentinel.0.try_wait().unwrap().is_none());
}

#[test]
fn blocked_required_observer_cannot_hold_principal_or_descendant_cancellation() {
    let root = root();
    let context = context(&root);
    struct CancelOnDrop(OperationContext);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let _cancel = CancelOnDrop(context.clone());
    let destination = root.join("installed");
    fs::write(&destination, "old-installed").unwrap();
    let staging = Arc::new(crate::install::source::Staging::new(&destination).unwrap());
    let source = staging.producer_path().to_owned();
    let weak = Arc::downgrade(&staging);
    let mut scope = staging.producer_scope(&context);
    let mut sentinel = Sentinel::start();
    let reached = Arc::new(AtomicBool::new(false));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let releasing = Release(Arc::clone(&release));
    let settled = Arc::new(AtomicBool::new(false));
    let mut step = scope
        .prepare(
            "/bin/sh",
            Purpose::Probe,
            "blocked required observer fixture",
        )
        .unwrap();
    step.command_mut().env_clear().env("PATH", "/usr/bin:/bin").current_dir(&root)
        .args(["-c", "printf partial > \"$1\"; printf %s $$ > principal.pid; /bin/sh -c 'printf %s $$ > descendant.pid; exec sleep 30' & wait", "fixture"])
        .arg(&source);
    let marker = root.join("descendant.pid");
    step.before_continue = Some(native::AdmissionPause {
        reached: Arc::new(AtomicBool::new(false)),
        release: Arc::new(AtomicBool::new(true)),
        settled: Arc::clone(&settled),
        client_marker: marker.clone(),
    });
    let observed = Arc::clone(&reached);
    let held = Arc::clone(&release);
    let mut first = true;
    step.observation = Some(Box::new(move || {
        if first {
            first = false;
            return Ok(());
        }
        wait_file(&marker);
        observed.store(true, Ordering::Release);
        let (lock, wake) = &*held;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = wake.wait(released).unwrap();
        }
        Ok(())
    }));
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = scope.run(step);
        drop(scope);
        let _ = send.send(result);
    });
    wait_file(&root.join("principal.pid"));
    wait_file(&root.join("descendant.pid"));
    let wait_pid = |name: &str| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pid) = fs::read_to_string(root.join(name))
                .ok()
                .and_then(|text| text.parse().ok())
            {
                break pid;
            }
            assert!(
                Instant::now() < deadline,
                "incomplete actual fixture PID handshake"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    let principal = pin(wait_pid("principal.pid"));
    let descendant = pin(wait_pid("descendant.pid"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !reached.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    context.cancel();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !settled.load(Ordering::Acquire) || !exited(&principal) || !exited(&descendant) {
        assert!(
            Instant::now() < deadline,
            "observer held actual producer cancellation or base quiescence"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !*release.0.lock().unwrap(),
        "observer was released before actual tree settlement"
    );
    assert!(sentinel.0.try_wait().unwrap().is_none());
    let abort = match receive.recv_timeout(Duration::from_secs(8)).unwrap() {
        Err(InstallFailure::Abort(abort)) => abort,
        _ => panic!("blocked required observation became a successful settled step"),
    };
    assert!(!abort.capture_complete);
    drop(staging);
    assert!(
        weak.upgrade().is_some(),
        "blocked observer lost the actual stage owner"
    );
    assert!(source.exists());
    drop(releasing);
    let deadline = Instant::now() + Duration::from_secs(5);
    while weak.upgrade().is_some() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        source.exists(),
        "late observer completion removed raw evidence after an uncertain return"
    );
    assert_eq!(fs::read_to_string(destination).unwrap(), "old-installed");
    assert!(sentinel.0.try_wait().unwrap().is_none());
    sentinel.0.stop().unwrap();
    fs::write(root.join("blocked-observer-cleanup-confirmed"), "actual principal, descendant and guardian quiesced before observer release; sentinel stopped separately").unwrap();
}

#[test]
fn operation_review_cancelled_and_expired_local_copies_must_not_publish() {
    let root = root();
    let mut violations = Vec::new();
    for case in ["cancelled", "expired"] {
        let directory = root.join(case);
        fs::create_dir(&directory).unwrap();
        let mut context = context(&directory);
        if case == "cancelled" {
            context.cancel();
        } else {
            context.deadline = Instant::now() - Duration::from_millis(1);
        }
        let source = directory.join("source");
        let destination = directory.join("installed");
        fs::write(&source, "new fixture bytes").unwrap();
        fs::write(&destination, "old-installed").unwrap();
        let result = crate::install::source::install_input_in(
            &context,
            &crate::install::source::Input::pin_local(source).unwrap(),
            &destination,
            crate::install::source::Options {
                kind: "offline operation-liveness fixture",
                limit: 4096,
                checksum: None,
                executable: false,
            },
            |_, _| Ok(()),
            crate::install::source::Publication::Replace,
        );
        if !matches!(result, Err(InstallFailure::Abort(_)))
            || fs::read_to_string(&destination).unwrap() != "old-installed"
        {
            violations.push(case);
        }
    }
    fs::write(
        root.join("local-copy-operation-review-confirmed"),
        format!("violations: {violations:?}"),
    )
    .unwrap();
    assert!(
        violations.is_empty(),
        "local publication ignored cancelled/expired operation: {violations:?}"
    );
}

#[test]
fn operation_review_previously_settled_permission_must_not_survive_cancellation() {
    let root = root();
    let context = context(&root);
    let lease = ProducerLease::default();
    let permission = ProcessScope::new(&context, lease.clone()).finish().unwrap();
    context.cancel();
    assert!(
        matches!(
            permission.consume_for(&lease.state),
            Err(InstallFailure::Abort(_))
        ),
        "cancelled operation retained a publish/cleanup permission"
    );
}

#[test]
fn operation_review_actual_required_capture_abort_must_stop_another_owner_lane() {
    let root = root();
    let context = context(&root);
    let first = ProducerDirectory::open(&root).unwrap();
    let mut scope = first.scope(&context);
    let mut step = scope
        .prepare("/bin/sh", Purpose::Probe, "actual required-capture abort")
        .unwrap();
    step.command_mut()
        .args(["-c", "printf too-much-required-output"]);
    step.capture(Capture::Inspect {
        stream_bytes: 1,
        total_bytes: 1,
    });
    assert!(matches!(scope.run(step), Err(InstallFailure::Abort(_))));
    drop(scope);
    let directory = root.join("different-owner");
    fs::create_dir(&directory).unwrap();
    let next = ProducerDirectory::open(&directory).unwrap();
    let mut scope = next.scope(&context);
    let mut step = scope
        .prepare("/bin/sh", Purpose::Probe, "forbidden later owner lane")
        .unwrap();
    step.command_mut()
        .current_dir(&root)
        .args(["-c", "printf forbidden > later-admission"]);
    let result = scope.run(step);
    if result.is_ok() {
        next.consume(scope.finish().unwrap()).unwrap();
    }
    let denied = matches!(result, Err(InstallFailure::Abort(_)));
    fs::write(
        root.join("later-lane-operation-review-confirmed"),
        "both attempted native helpers settled or were never admitted",
    )
    .unwrap();
    assert!(
        denied && !root.join("later-admission").exists(),
        "required capture abort did not stop a different actual owner lane"
    );
}

#[test]
fn checked_publication_rejects_late_cancel_and_expiry_for_both_commit_variants() {
    use crate::install::source::{
        FaultPoint, Input, Options, Publication, faults, install_input_in,
    };
    let root = root();
    for publication in [Publication::Replace, Publication::NoClobber] {
        for expire in [false, true] {
            let directory = root.join(format!(
                "{}-{expire}",
                if matches!(publication, Publication::Replace) {
                    "replace"
                } else {
                    "no-clobber"
                }
            ));
            fs::create_dir(&directory).unwrap();
            let mut context = context(&directory);
            if expire {
                context.deadline = Instant::now() + Duration::from_millis(500);
            }
            let destination = directory.join("installed");
            let source = directory.join("source");
            fs::write(&source, "new product").unwrap();
            if matches!(publication, Publication::Replace) {
                fs::write(&destination, "old product").unwrap();
            }
            let operation = context.clone();
            let reached = Arc::new(AtomicBool::new(false));
            let observed = Arc::clone(&reached);
            let _hook = faults::set(FaultPoint::CommitRename, move |_| {
                observed.store(true, Ordering::Release);
                if expire {
                    while Instant::now() < operation.deadline {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                } else {
                    operation.cancel();
                }
                Ok(())
            });
            let result = install_input_in(
                &context,
                &Input::pin_local(source).unwrap(),
                &destination,
                Options {
                    kind: "late publication fixture",
                    limit: 4096,
                    checksum: None,
                    executable: false,
                },
                |_, _| Ok(()),
                publication,
            );
            assert!(
                reached.load(Ordering::Acquire),
                "commit hook was not reached"
            );
            assert!(matches!(result, Err(InstallFailure::Abort(_))));
            if matches!(publication, Publication::Replace) {
                assert_eq!(fs::read(&destination).unwrap(), b"old product");
            } else {
                assert!(!destination.exists());
            }
            // Both detached raw bytes and the quiet candidate remain inspectable.
            let retained: Vec<_> = fs::read_dir(&directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().unwrap().is_dir())
                .collect();
            assert!(
                retained.len() >= 2,
                "raw/candidate namespace was removed on typed rejection"
            );
            let products = retained
                .iter()
                .flat_map(|entry| fs::read_dir(entry.path()).unwrap())
                .filter_map(Result::ok)
                .filter(|entry| fs::read(entry.path()).ok().as_deref() == Some(b"new product"))
                .count();
            assert!(products >= 2, "raw/candidate bytes were not both retained");
        }
    }
}

#[test]
fn consumed_commit_guard_rejects_a_later_attempt_on_the_same_actual_owner() {
    let root = root();
    let context = context(&root);
    let stage = Arc::new(crate::install::source::Staging::new(&root.join("installed")).unwrap());
    let permission = stage.producer_scope(&context).finish().unwrap();
    let guard = stage.accept_producer_permission(permission).unwrap();
    assert!(guard.check().is_ok());
    let mut scope = stage.producer_scope(&context);
    let mut step = scope
        .prepare(
            "/bin/sh",
            Purpose::Probe,
            "forbidden post-consumption producer",
        )
        .unwrap();
    step.command_mut()
        .current_dir(&root)
        .args(["-c", "printf forbidden > forbidden"]);
    assert!(matches!(scope.run(step), Err(InstallFailure::Abort(_))));
    assert!(matches!(guard.check(), Err(InstallFailure::Abort(_))));
    assert!(!root.join("forbidden").exists());
}

#[test]
fn ordinary_preexec_failure_must_retain_actual_stage_after_observer_release() {
    use std::os::unix::{net::UnixStream, process::CommandExt};
    let root = root();
    let context = context(&root);
    let source = root.join("input");
    let destination = root.join("installed");
    fs::write(&source, "new fixture bytes").unwrap();
    fs::write(&destination, "old installed bytes").unwrap();
    let mut sentinel = Sentinel::start();
    let reached = Arc::new(AtomicBool::new(false));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let releasing = Release(Arc::clone(&release));
    let (read, mut write) = UnixStream::pair().unwrap();
    write
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let (owner_send, owner_receive) = mpsc::sync_channel(1);
    let (send, receive) = mpsc::sync_channel(1);
    let observed = Arc::clone(&reached);
    let held = Arc::clone(&release);
    let target = destination.clone();
    let worker = std::thread::spawn(move || {
        let command_root = context.root.clone();
        let result = crate::install::source::install_input_in(
            &context,
            &crate::install::source::Input::pin_local(source).unwrap(),
            &target,
            crate::install::source::Options {
                kind: "ordinary preexec custody fixture",
                limit: 4096,
                checksum: None,
                executable: false,
            },
            move |path, scope| {
                let completion = scope
                    .lease
                    ._owner
                    .as_ref()
                    .unwrap()
                    .drop_completion()
                    .unwrap();
                owner_send.send((path.to_owned(), completion)).unwrap();
                let mut step = scope.prepare("/bin/sh", Purpose::Probe, "preexec failure")?;
                step.command_mut()
                    .current_dir(&command_root)
                    .args(["-c", "printf forbidden > forbidden-client"]);
                let descriptor = read.as_raw_fd();
                // SAFETY: this child callback uses only async-signal-safe read
                // and constructs the fixed OS error. No mutex, allocator, or
                // test framework code executes between fork and failed exec.
                unsafe {
                    step.command_mut().pre_exec(move || {
                        let mut byte = 0u8;
                        loop {
                            let result = libc::read(descriptor, (&mut byte as *mut u8).cast(), 1);
                            if result >= 0 {
                                break;
                            }
                            if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                                break;
                            }
                        }
                        Err(io::Error::from_raw_os_error(libc::EIO))
                    });
                }
                let mut first = true;
                step.observation = Some(Box::new(move || {
                    if first {
                        first = false;
                        return Ok(());
                    }
                    observed.store(true, Ordering::Release);
                    let (lock, wake) = &*held;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = wake.wait(released).unwrap();
                    }
                    Ok(())
                }));
                let result = scope.run(step).map(drop);
                drop(read);
                result
            },
            crate::install::source::Publication::Replace,
        );
        let _ = send.send(result);
    });
    let (raw, completion) = owner_receive.recv_timeout(Duration::from_secs(5)).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while !reached.load(Ordering::Acquire) {
        if Instant::now() >= end {
            // Even a failed handshake must release only this owned preexec.
            let _ = write.write_all(&[1]);
            panic!("required observer did not enter its held second sample");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    write.write_all(&[1]).unwrap();
    let result = receive.recv_timeout(Duration::from_secs(8)).unwrap();
    worker.join().unwrap();
    let was_abort = matches!(result, Err(InstallFailure::Abort(_)));
    let retained_before = raw.exists();
    assert!(sentinel.0.try_wait().unwrap().is_none());
    drop(releasing);
    let end = Instant::now() + Duration::from_secs(5);
    while !completion.load(Ordering::Acquire) {
        assert!(
            Instant::now() < end,
            "actual Stage destructor did not finish"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let sentinel_alive_after_release = sentinel.0.try_wait().unwrap().is_none();
    sentinel.0.stop().unwrap();
    let sentinel_settled = sentinel.0.try_wait().unwrap().is_some();
    fs::write(root.join("ordinary-observer-review-confirmed"),
        "real preexec failed without client admission; held observer released; actual owner dropped; unrelated sentinel stopped separately").unwrap();
    assert!(sentinel_alive_after_release && sentinel_settled);
    assert!(
        was_abort && retained_before && raw.exists(),
        "ordinary spawn failure's observer deleted actual raw after reported custody Abort"
    );
    assert_eq!(fs::read(destination).unwrap(), b"old installed bytes");
    assert!(!root.join("forbidden-client").exists());
}

#[test]
fn local_validation_abort_and_cancel_must_retain_raw_after_actual_stage_drop() {
    let root = root();
    let mut violations = Vec::new();
    for explicit_abort in [false, true] {
        let directory = root.join(if explicit_abort {
            "explicit-abort"
        } else {
            "cancel"
        });
        fs::create_dir(&directory).unwrap();
        let context = context(&directory);
        let source = directory.join("input");
        let destination = directory.join("installed");
        fs::write(&source, "new local bytes").unwrap();
        fs::write(&destination, "old local bytes").unwrap();
        let mut owned = None;
        let result = crate::install::source::install_input_in(
            &context,
            &crate::install::source::Input::pin_local(source).unwrap(),
            &destination,
            crate::install::source::Options {
                kind: "local validation abort fixture",
                limit: 4096,
                checksum: None,
                executable: false,
            },
            |path, scope| {
                let completion = scope
                    .lease
                    ._owner
                    .as_ref()
                    .unwrap()
                    .drop_completion()
                    .unwrap();
                owned = Some((path.to_owned(), completion));
                if explicit_abort {
                    Err(context.custody_failure("explicit validation abort fixture", path))
                } else {
                    context.cancel();
                    Ok(())
                }
            },
            crate::install::source::Publication::Replace,
        );
        let (raw, completion) = owned.unwrap();
        assert!(
            completion.load(Ordering::Acquire),
            "local Stage destructor did not finish"
        );
        if !matches!(result, Err(InstallFailure::Abort(_))) || !raw.exists() {
            violations.push(if explicit_abort {
                "explicit-abort"
            } else {
                "cancel"
            });
        }
        assert_eq!(fs::read(destination).unwrap(), b"old local bytes");
    }
    fs::write(
        root.join("local-abort-drop-confirmed"),
        format!("violations: {violations:?}"),
    )
    .unwrap();
    assert!(
        violations.is_empty(),
        "local typed Abort deleted actual raw after final Drop: {violations:?}"
    );
}

#[test]
fn prior_ordinary_detail_composition_preserves_typed_failure_facts() {
    let root = root();
    let context = context(&root);
    let prior = crate::Error::Input("specific prior reason".into());
    for detail in [None, Some("existing custody detail".to_owned())] {
        for include_prior in [false, true] {
            let original = AbortInstallation {
                cause: AbortCause::Custody,
                cleanup_confirmed: false,
                capture_complete: true,
                label: "unchanged step".into(),
                config_path: context.config_path.clone(),
                limit_key: "operation_timeout_seconds",
                evidence: Some(root.join("raw")),
                detail: detail.clone(),
            };
            let failure = InstallFailure::Abort(original.clone())
                .with_prior_ordinary(include_prior.then_some(&prior));
            let InstallFailure::Abort(actual) = failure else {
                panic!("composition erased typed Abort");
            };
            assert_eq!(actual.cause, original.cause);
            assert_eq!(actual.cleanup_confirmed, original.cleanup_confirmed);
            assert_eq!(actual.capture_complete, original.capture_complete);
            assert_eq!(actual.label, original.label);
            assert_eq!(actual.config_path, original.config_path);
            assert_eq!(actual.limit_key, original.limit_key);
            assert_eq!(actual.evidence, original.evidence);
            if include_prior {
                let text = actual.detail.unwrap();
                assert_eq!(text.matches("specific prior reason").count(), 1);
                if let Some(detail) = &detail {
                    assert!(text.starts_with(detail));
                }
            } else {
                assert_eq!(actual.detail, original.detail);
            }
        }
    }
    for include_prior in [false, true] {
        let failure = InstallFailure::Ordinary(crate::Error::Input("unchanged ordinary".into()))
            .with_prior_ordinary(include_prior.then_some(&prior));
        assert!(matches!(&failure, InstallFailure::Ordinary(_)));
        assert_eq!(failure.to_string(), "unchanged ordinary");
    }
}

#[test]
fn ordinary_validation_reason_survives_later_operation_abort() {
    let root = root();
    let context = context(&root);
    let source = root.join("input");
    let destination = root.join("installed");
    fs::write(&source, "new local bytes").unwrap();
    fs::write(&destination, "old local bytes").unwrap();
    let mut owned = None;
    let result = crate::install::source::install_input_in(
        &context,
        &crate::install::source::Input::pin_local(source).unwrap(),
        &destination,
        crate::install::source::Options {
            kind: "primary failure preservation fixture",
            limit: 4096,
            checksum: None,
            executable: false,
        },
        |path, scope| {
            let completion = scope
                .lease
                ._owner
                .as_ref()
                .unwrap()
                .drop_completion()
                .unwrap();
            owned = Some((path.to_owned(), completion));
            context.cancel();
            Err(crate::Error::Input("specific original validation reason".into()).into())
        },
        crate::install::source::Publication::Replace,
    );
    let (raw, completion) = owned.unwrap();
    assert!(
        completion.load(Ordering::Acquire),
        "actual Stage Drop is pending"
    );
    assert!(raw.is_file(), "operation Abort erased retained raw staging");
    assert_eq!(fs::read(destination).unwrap(), b"old local bytes");
    let error = result.expect_err("cancelled validation published its source");
    assert!(
        matches!(error, InstallFailure::Abort(_)),
        "Abort became an ordinary fallback"
    );
    assert!(
        error
            .to_string()
            .contains("specific original validation reason"),
        "later operation Abort erased the original validation error: {error}"
    );
}

#[test]
fn operation_reports_one_bounded_diagnostic_summary_and_drains_it_once() {
    let root = root();
    let context = context(&root);
    for index in 0..130 {
        context.record_log(&native::LogReport {
            path: Some(root.join(format!("private-step-{index}/output.log"))),
            retained: 3,
            accepted: 5,
            discarded: 2,
            fault: (index % 2 == 0).then_some(LogFault {
                phase: LogPhase::Write,
                kind: io::ErrorKind::WriteZero,
            }),
            settled: index % 2 == 0,
            truncated: true,
        });
    }
    let mut output = Vec::new();
    context.report_diagnostics(&mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert_eq!(output.matches("Warning:").count(), 1);
    assert_eq!(output.lines().count(), 130); // One heading,128 paths,one omitted count.
    assert!(output.contains("private-step-0/output.log: 3 bytes retained, 5 bytes accepted, 2 bytes discarded; writer settled; log write failed (WriteZero)"));
    assert!(output.contains("private-step-1/output.log: 3 bytes retained, 5 bytes accepted, 2 bytes discarded; writer pending (completion unconfirmed)"));
    assert!(output.contains("2 additional diagnostic log snapshots omitted"));
    assert!(!output.contains("private-step-128/"));
    assert!(!output.contains("private-step-129/"));
    assert!(!output.contains("capture complete"));
    let mut second = Vec::new();
    context.report_diagnostics(&mut second).unwrap();
    assert!(second.is_empty());
}

struct FaultReader {
    inner: Box<dyn std::io::Read + Send>,
    panic: bool,
    fired: bool,
}
impl std::io::Read for FaultReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if !self.fired {
            self.fired = true;
            assert!(!self.panic, "private synthetic pipe read panic");
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        self.inner.read(bytes)
    }
}

#[test]
fn migrated_source_callback_faults_cannot_continue_publish_or_destroy_raw() {
    let root = root();
    let mut sentinel = Sentinel::start();
    for fault in ["read", "panic", "overflow", "owner-loss"] {
        let directory = root.join(fault);
        fs::create_dir(&directory).unwrap();
        let context = context(&directory);
        let source = directory.join("input");
        let destination = directory.join("installed");
        fs::write(&source, "new source bytes").unwrap();
        fs::write(&destination, "old installation bytes").unwrap();
        let mut owner = None;
        let result = crate::install::source::install_input_in(
            &context,
            &crate::install::source::Input::pin_local(source).unwrap(),
            &destination,
            crate::install::source::Options {
                kind: "migrated source callback fault fixture",
                limit: 4096,
                checksum: None,
                executable: false,
            },
            |path, scope| {
                owner = Some((
                    path.to_owned(),
                    scope
                        .lease
                        ._owner
                        .as_ref()
                        .unwrap()
                        .drop_completion()
                        .unwrap(),
                ));
                let mut step =
                    scope.prepare("/bin/sh", Purpose::Version, "migrated source validation")?;
                step.command_mut().current_dir(&directory).env_clear().env("PATH", "/usr/bin:/bin")
                    .args(["-c", "printf %s $$ > principal.pid; /bin/sh -c 'printf %s $$ > descendant.pid; exec /bin/sleep 30' & while ! test -s descendant.pid; do /bin/sleep 0.001; done; if test \"$2\" = owner-loss; then /bin/rm -- \"$1\"; printf foreign-owner-arrival > \"$1\"; else printf too-much-data; fi; wait", "fixture"])
                    .arg(path).arg(fault);
                step.before_continue = Some(native::AdmissionPause {
                    reached: Arc::new(AtomicBool::new(false)),
                    release: Arc::new(AtomicBool::new(true)),
                    settled: Arc::new(AtomicBool::new(false)),
                    client_marker: directory.join("descendant.pid"),
                });
                match fault {
                    "read" | "panic" => {
                        let panic = fault == "panic";
                        step.reader_factory = Some(Box::new(move |inner| {
                            Box::new(FaultReader {
                                inner,
                                panic,
                                fired: false,
                            })
                        }));
                    }
                    "overflow" => step.capture(Capture::Inspect {
                        stream_bytes: 1,
                        total_bytes: 1,
                    }),
                    _ => {}
                }
                scope.run(step)?;
                fs::write(
                    directory.join("forbidden-callback-continuation"),
                    "must not reach",
                )
                .map_err(crate::io_error)?;
                Ok(())
            },
            crate::install::source::Publication::Replace,
        );
        let abort = match result {
            Err(InstallFailure::Abort(abort)) => abort,
            _ => panic!("{fault}: migrated callback converted uncertainty into ordinary/success"),
        };
        assert_eq!(
            abort.cause,
            match fault {
                "read" => AbortCause::CaptureRead,
                "panic" => AbortCause::Worker,
                "overflow" => AbortCause::CaptureOverflow,
                _ => AbortCause::OwnerObservation,
            }
        );
        assert!(
            abort.cleanup_confirmed,
            "{fault}: no actual matching tree settlement"
        );
        assert!(!abort.capture_complete);
        if fault == "read" {
            assert!(abort.detail.as_deref().unwrap().contains(&format!(
                "capture pipe read/configuration failed ({:?})",
                io::Error::from_raw_os_error(libc::EIO).kind()
            )));
        }
        assert!(
            !fs::read(directory.join("descendant.pid"))
                .unwrap()
                .is_empty()
        );
        let (raw, completion) = owner.unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while !completion.load(Ordering::Acquire) {
            assert!(
                Instant::now() < end,
                "{fault}: actual Stage Drop did not finish"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            raw.exists(),
            "{fault}: raw destroyed after final owner Drop"
        );
        if fault == "owner-loss" {
            assert_eq!(fs::read(raw).unwrap(), b"foreign-owner-arrival");
        }
        assert_eq!(fs::read(destination).unwrap(), b"old installation bytes");
        assert!(!directory.join("forbidden-callback-continuation").exists());
        assert!(sentinel.0.try_wait().unwrap().is_none());
    }
    sentinel.0.stop().unwrap();
}

#[test]
fn migrated_source_worker_start_failure_is_known_never_admitted_not_unknown_success() {
    let root = root();
    let context = context(&root);
    let source = root.join("input");
    let destination = root.join("installed");
    fs::write(&source, "new bytes").unwrap();
    fs::write(&destination, "old bytes").unwrap();
    let mut owner = None;
    let result = crate::install::source::install_input_in(
        &context,
        &crate::install::source::Input::pin_local(source).unwrap(),
        &destination,
        crate::install::source::Options {
            kind: "migrated never-admitted fixture",
            limit: 4096,
            checksum: None,
            executable: false,
        },
        |path, scope| {
            owner = Some((
                path.to_owned(),
                scope
                    .lease
                    ._owner
                    .as_ref()
                    .unwrap()
                    .drop_completion()
                    .unwrap(),
            ));
            let mut step = scope.prepare("/bin/sh", Purpose::Version, "worker start failure")?;
            step.command_mut()
                .current_dir(&root)
                .args(["-c", "printf forbidden > forbidden-client"]);
            step.fail_worker_start = Some(io::ErrorKind::Other);
            scope.run(step).map(drop)
        },
        crate::install::source::Publication::Replace,
    );
    assert!(matches!(&result, Err(InstallFailure::Ordinary(_))));
    assert!(
        result
            .err()
            .unwrap()
            .into_public()
            .to_string()
            .contains("custody worker")
    );
    let (raw, completion) = owner.unwrap();
    assert!(completion.load(Ordering::Acquire));
    assert!(
        !raw.exists(),
        "known never-admitted cleanup should remain supported"
    );
    assert_eq!(fs::read(destination).unwrap(), b"old bytes");
    assert!(!root.join("forbidden-client").exists());
}

#[test]
fn actual_duplex_binary_capture_and_stdin_preserve_configured_command() {
    let root = root();
    let context = context(&root);
    let input = root.join("binary-input");
    let bytes = [0xff, 0, 0x80, b'\n'].repeat(32768);
    fs::write(&input, &bytes).unwrap();
    let python = crate::tools::host_executable(Path::new("python3")).unwrap();
    let mut step = PreparedStep::new(
        &context,
        python,
        Purpose::Probe,
        "offline duplex binary fixture",
    )
    .unwrap();
    step.command_mut().env_clear().env("REMOVED", "must not leak").env_remove("REMOVED")
        .current_dir(&root).args(["-c", "import os,sys; assert 'REMOVED' not in os.environ; data=sys.stdin.buffer.read(); sys.stdout.buffer.write(b'A'*262144+data); sys.stdout.buffer.flush(); sys.stderr.buffer.write(b'B'*262144); sys.stderr.buffer.flush()"])
        .stdin(std::process::Stdio::from(fs::File::open(input).unwrap()));
    step.capture(Capture::inspection());
    let result = step.run(ProducerLease::default()).unwrap();
    assert!(result.status().success());
    assert_eq!(&result.stdout()[..262144], vec![b'A'; 262144]);
    assert_eq!(&result.stdout()[262144..], bytes);
    assert_eq!(result.stderr(), vec![b'B'; 262144]);
    assert!(std::str::from_utf8(result.stdout()).is_err());
}

struct AncillaryFaultWriter {
    file: fs::File,
    flush_failure: bool,
    observed: Arc<AtomicBool>,
}
impl Write for AncillaryFaultWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.flush_failure {
            self.file.write(bytes)
        } else {
            self.observed.store(true, Ordering::Release);
            Ok(0)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.observed.store(true, Ordering::Release);
        if self.flush_failure {
            Err(io::Error::from_raw_os_error(libc::ENOSPC))
        } else {
            Ok(())
        }
    }
}

#[test]
fn actual_source_installation_succeeds_despite_ancillary_write_or_flush_failure() {
    let root = root();
    for flush_failure in [false, true] {
        let directory = root.join(if flush_failure { "flush" } else { "write" });
        fs::create_dir(&directory).unwrap();
        let context = context(&directory);
        let source = directory.join("input");
        let destination = directory.join("installed");
        fs::write(&source, "valid product bytes").unwrap();
        let observed = Arc::new(AtomicBool::new(false));
        let receipt = Arc::clone(&observed);
        crate::install::source::install_input_in(
            &context,
            &crate::install::source::Input::pin_local(source).unwrap(),
            &destination,
            crate::install::source::Options {
                kind: "healthy installation with ancillary failure",
                limit: 4096,
                checksum: None,
                executable: false,
            },
            |_, scope| {
                let mut step = scope.prepare("/bin/sh", Purpose::Version, "valid product probe")?;
                step.command_mut()
                    .args(["-c", "printf inert-private-diagnostic; exit 0"]);
                step.log_writer = Some(Box::new(move |file| {
                    Box::new(AncillaryFaultWriter {
                        file,
                        flush_failure,
                        observed,
                    })
                }));
                assert!(scope.run(step)?.status.success());
                Ok(())
            },
            crate::install::source::Publication::Replace,
        )
        .unwrap();
        assert_eq!(fs::read(destination).unwrap(), b"valid product bytes");
        let end = Instant::now() + Duration::from_secs(5);
        while !receipt.load(Ordering::Acquire) {
            assert!(
                Instant::now() < end,
                "actual ancillary fault was not reached"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut output = Vec::new();
        context.report_diagnostics(&mut output).unwrap();
        assert!(
            !String::from_utf8(output)
                .unwrap()
                .contains("inert-private-diagnostic")
        );
    }
}

#[test]
fn actual_source_installation_and_required_capture_survive_exhausted_diagnostic_budget() {
    let root = root();
    let mut context = context(&root);
    context.log_budget = Arc::new(native::LogBudget::with_limits(0, 0));
    let source = root.join("input");
    let destination = root.join("installed");
    fs::write(&source, "valid product").unwrap();
    crate::install::source::install_input_in(
        &context,
        &crate::install::source::Input::pin_local(source).unwrap(),
        &destination,
        crate::install::source::Options {
            kind: "diagnostic budget independence fixture",
            limit: 4096,
            checksum: None,
            executable: false,
        },
        |_, scope| {
            let mut step = scope.prepare(
                "/bin/sh",
                Purpose::Version,
                "required inspection despite log quota",
            )?;
            step.command_mut()
                .args(["-c", "printf required-inert-response"]);
            step.capture(Capture::inspection());
            let result = scope.run(step)?;
            assert!(result.status.success());
            assert_eq!(result.stdout, b"required-inert-response");
            assert!(result.log.settled);
            assert_eq!(result.log.fault.unwrap().phase, LogPhase::Budget);
            Ok(())
        },
        crate::install::source::Publication::Replace,
    )
    .unwrap();
    assert_eq!(fs::read(destination).unwrap(), b"valid product");
    let mut output = Vec::new();
    context.report_diagnostics(&mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert_eq!(output.matches("Warning:").count(), 1);
    assert!(output.contains("diagnostic log budget failed (WouldBlock)"));
    assert!(!output.contains("required-inert-response"));
    assert!(!fs::read_dir(&root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".asb-install-step-")
    }));
}
