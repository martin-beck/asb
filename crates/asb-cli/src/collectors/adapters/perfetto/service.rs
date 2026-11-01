//! Owned manual Tracebox services shared by concurrent sessions, never autostart.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static SERVICES: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Service>>>> = OnceLock::new();

#[cfg(test)]
#[path = "probe_tests.rs"]
mod probe_tests;

pub(super) struct Service {
    pub instance: String,
    pub marker: PathBuf,
    executable: PathBuf,
    producer: String,
    consumer: String,
    children: Arc<Mutex<Vec<Child>>>,
    owner: Option<Owner>,
    // The probe sidecar is launched in a separate namespace and can outlive
    // the caller which started it.  Keep an execution-pinned copy rather than
    // referring to Cargo's mutable target executable: a rebuild must never
    // turn an active benchmark into ".../asb (deleted)".
    namespace_helper: Option<PathBuf>,
    _lease: File,
}

struct Owner {
    stop: mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

struct Daemons {
    children: Arc<Mutex<Vec<Child>>>,
    readers: Vec<std::thread::JoinHandle<()>>,
}

impl Service {
    pub fn acquire(executable: &Path, project_root: &Path) -> Result<Arc<Self>, String> {
        let root = project_root.canonicalize().map_err(|e| e.to_string())?;
        let mut services = SERVICES
            .get_or_init(|| Mutex::new(BTreeMap::new()))
            .lock()
            .map_err(|_| "Perfetto service registry is unavailable")?;
        let _admission = super::project::lock_admission_for(&root, Duration::from_secs(15))
            .map_err(|e| e.to_string())?;
        // Reset can finish between initial project discovery and admission.
        // Reload under the stable lock and never fall back to a parent project.
        let (current_root, settings) = crate::project::load(&root).map_err(|e| e.to_string())?;
        if current_root.canonicalize().map_err(|e| e.to_string())? != root {
            return Err("benchmark project changed during Perfetto admission; retry after initialization completes".into());
        }
        if settings.perfetto_instances.is_empty() {
            return Err("Perfetto needs delegated, isolated tracefs instances. Run `asb init --configure-host` in this benchmark project and approve the printed Perfetto commands (or use --yes after reviewing them). Existing project data will not be reset.".into());
        }
        let expected = super::project::instance_names(&root)?;
        if settings
            .perfetto_instances
            .iter()
            .any(|name| !expected.contains(name))
        {
            return Err("Perfetto instance configuration does not belong to this project. Run `asb init --configure-host` to create its own instances; do not reuse another project's tracing instance.".into());
        }
        if let Some(service) = services.get(&root).and_then(Weak::upgrade) {
            if service.executable != executable {
                return Err("concurrent Perfetto sessions selected different executables; stop them before changing the tool installation".into());
            }
            service.check_alive()?;
            return Ok(service);
        }
        let directory = root.join(".asb/perfetto");
        fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
        let tracefs = if Path::new("/sys/kernel/tracing/instances").is_dir() {
            Path::new("/sys/kernel/tracing")
        } else {
            Path::new("/sys/kernel/debug/tracing")
        };
        for instance in settings.perfetto_instances {
            use std::os::unix::fs::OpenOptionsExt;
            let lease_path = directory.join(format!("{instance}.lease"));
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&lease_path)
                .map_err(|e| format!("cannot open Perfetto instance lease: {e}"))?;
            if lock.try_lock().is_err() {
                continue;
            }
            // A reset/reconfiguration may have replaced a lease pathname
            // after open but before lock. Never start against an unlinked old
            // inode while another process can lock the new coordination file.
            use std::os::unix::fs::MetadataExt;
            let opened = lock.metadata().map_err(|e| e.to_string())?;
            let current = fs::metadata(&lease_path).map_err(|_| "Perfetto lease changed during project reset; retry after initialization completes")?;
            if opened.nlink() == 0 || opened.ino() != current.ino() || opened.dev() != current.dev()
            {
                return Err("Perfetto lease changed during project reset; retry after initialization completes".into());
            }
            let marker = tracefs
                .join("instances")
                .join(&instance)
                .join("trace_marker");
            let instance_root = marker.parent().expect("instance marker parent");
            let uid = fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid();
            if !fs::symlink_metadata(instance_root)
                .is_ok_and(|metadata| metadata.is_dir() && metadata.uid() == uid)
                || !super::project::delegated_ready(instance_root)
            {
                return Err("Perfetto instance delegation is incomplete or foreign-owned; run `asb init --configure-host` and review the exact scoped repair commands. No instance was modified by benchmarking.".into());
            }
            OpenOptions::new().write(true).open(&marker).map_err(|e| format!("delegated Perfetto instance is missing or not writable ({e}); run `asb init --configure-host` after reboot or repair the printed instance permissions"))?;
            let helper = std::env::current_exe().map_err(|e| e.to_string())?;
            #[cfg(test)]
            let helper = std::env::var_os("ASB_TEST_PERFETTO_HELPER")
                .map(PathBuf::from)
                .unwrap_or(helper);
            let service = Arc::new(Self::start(
                executable,
                instance,
                marker,
                lock,
                &directory,
                Some(&helper),
            )?);
            service.check_alive()?;
            services.insert(root, Arc::downgrade(&service));
            return Ok(service);
        }
        Err("all of this project's Perfetto instances are leased by other ASB processes. Wait for one of those runs to finish, or use a separately initialized benchmark project; no running capture was stopped.".into())
    }

    fn start(
        executable: &Path,
        instance: String,
        marker: PathBuf,
        lease: File,
        directory: &Path,
        namespace_helper: Option<&Path>,
    ) -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let producer = format!("@asb-perfetto-p-{}-{nonce:x}", std::process::id());
        let consumer = format!("@asb-perfetto-c-{}-{nonce:x}", std::process::id());
        let namespace_helper = namespace_helper
            .map(|helper| pin_namespace_helper(helper, directory, &instance, nonce))
            .transpose()?;
        let mut service = Self {
            instance,
            marker,
            executable: executable.to_path_buf(),
            producer,
            consumer,
            children: Arc::new(Mutex::new(Vec::new())),
            owner: None,
            namespace_helper,
            _lease: lease,
        };
        let mut traced = service.command("traced");
        let mut probes = service.command("traced_probes");
        if let Some(helper) = &service.namespace_helper {
            use std::os::fd::AsRawFd;
            probes = Command::new("unshare");
            probes
                .args([
                    "--user",
                    "--map-root-user",
                    "--mount",
                    "--propagation",
                    "private",
                    "--",
                ])
                .arg(helper)
                .args(["__asb-collector-service", "perfetto"])
                .arg(executable)
                .arg(service.marker.with_file_name("trace"))
                .arg(std::process::id().to_string())
                .arg(service._lease.as_raw_fd().to_string())
                .arg(
                    fs::read_link("/proc/self/ns/user")
                        .map_err(|e| format!("cannot identify owner user namespace: {e}"))?,
                )
                .arg(
                    fs::read_link("/proc/self/ns/mnt")
                        .map_err(|e| format!("cannot identify owner mount namespace: {e}"))?,
                )
                .env("PERFETTO_PRODUCER_SOCK_NAME", &service.producer)
                .env("PERFETTO_CONSUMER_SOCK_NAME", &service.consumer);
        }
        let mut query = service.command("perfetto");
        query.arg("--query");
        inherit_lease(&mut traced, &service._lease);
        inherit_lease(&mut probes, &service._lease);
        parent_death(&mut traced);
        parent_death(&mut probes);
        let producer = service.producer.clone();
        let consumer = service.consumer.clone();
        let directory = directory.to_path_buf();
        let instance = service.instance.clone();
        let children = Arc::clone(&service.children);
        let (stop, stopped) = mpsc::channel();
        let (ready, started) = mpsc::sync_channel(1);
        // Linux PDEATHSIG follows the spawning THREAD. This dedicated owner
        // outlives whichever replica acquired the service first and exits only
        // after the final session releases it (or the entire process dies).
        let thread = std::thread::Builder::new().name("asb-perfetto-owner".into()).spawn(move || {
            let mut daemons = Daemons { children, readers: Vec::new() };
            let result = (|| -> Result<(), String> {
                daemons.start(traced, &directory.join(format!("{instance}-{nonce:x}-traced.log")))?;
                let deadline = Instant::now() + Duration::from_secs(5);
                while !socket_ready(&producer) || !socket_ready(&consumer) {
                    daemons.check_alive()?;
                    if Instant::now() >= deadline { return Err("owned Perfetto service did not open private sockets within 5 seconds; inspect .asb/perfetto service logs".into()); }
                    std::thread::sleep(Duration::from_millis(25));
                }
                daemons.start(probes, &directory.join(format!("{instance}-{nonce:x}-probes.log")))?;
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    daemons.check_alive()?;
                    if producer_registered(&mut query)? { break; }
                    if Instant::now() >= deadline { return Err("manual traced_probes did not register linux.ftrace and linux.process_stats within 5 seconds; inspect .asb/perfetto logs and check unshare/user-mount namespace support".into()); }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(())
            })();
            let success = result.is_ok();
            let _ = ready.send(result);
            if success { let _ = stopped.recv(); }
            // Daemons are stopped/reaped and log readers joined before the
            // owner thread exits and before the instance lease is released.
        }).map_err(|e| format!("cannot start Perfetto owner thread: {e}"))?;
        service.owner = Some(Owner { stop, thread });
        started
            .recv_timeout(Duration::from_secs(12))
            .map_err(|_| "Perfetto service startup timed out or owner exited".to_owned())??;
        Ok(service)
    }

    pub fn command(&self, applet: &str) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .arg(applet)
            .env("PERFETTO_PRODUCER_SOCK_NAME", &self.producer)
            .env("PERFETTO_CONSUMER_SOCK_NAME", &self.consumer);
        command
    }

    pub fn collector_command(
        &self,
        applet: &str,
    ) -> Result<(Command, crate::launch::CommandRoute), String> {
        let mut launch = crate::launch::ProcessLaunch::new(&self.executable);
        launch.arg(applet);
        let (mut command, route) = crate::measurements::producer_command(&mut launch)?;
        command
            .env("PERFETTO_PRODUCER_SOCK_NAME", &self.producer)
            .env("PERFETTO_CONSUMER_SOCK_NAME", &self.consumer);
        Ok((command, route))
    }

    fn check_alive(&self) -> Result<(), String> {
        let mut children = self
            .children
            .lock()
            .map_err(|_| "Perfetto child ownership failed")?;
        for child in children.iter_mut() {
            if child.try_wait().map_err(|e| e.to_string())?.is_some() {
                return Err("an owned Perfetto daemon exited; inspect .asb/perfetto service logs. Existing captures were not restarted or replaced.".into());
            }
            #[cfg(target_os = "linux")]
            {
                use std::os::fd::AsRawFd;
                use std::os::unix::fs::MetadataExt;
                let expected = self._lease.metadata().map_err(|e| e.to_string())?;
                let inherited = fs::metadata(format!("/proc/{}/fd/{}", child.id(), self._lease.as_raw_fd()))
                    .map_err(|_| "Perfetto daemon did not retain its instance lease; refusing unsafe capture startup")?;
                if inherited.dev() != expected.dev() || inherited.ino() != expected.ino() {
                    return Err("Perfetto daemon's inherited instance lease changed; refusing unsafe capture startup".into());
                }
            }
        }
        Ok(())
    }
}

fn producer_registered(command: &mut Command) -> Result<bool, String> {
    use std::io::Read;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot query private Perfetto service: {e}"))?;
    let stdout = child.stdout.take().expect("piped query stdout");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.take(65537).read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(1);
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| "Perfetto query reader failed")?
        .map_err(|e| e.to_string())?;
    let output = String::from_utf8_lossy(&bytes);
    Ok(success
        && bytes.len() <= 65536
        && output.contains("linux.ftrace")
        && output.contains("linux.process_stats"))
}

impl Drop for Service {
    fn drop(&mut self) {
        let joined = if let Some(owner) = self.owner.take() {
            let _ = owner.stop.send(());
            owner.thread.join().is_ok()
        } else {
            true
        };
        let reaped = joined
            && self.children.lock().is_ok_and(|mut children| {
                children
                    .iter_mut()
                    .all(|child| matches!(child.try_wait(), Ok(Some(_))))
            });
        if reaped {
            // A concurrent fork can retain this open file description even
            // before exec closes its incidental copy. Once ALL owned daemons
            // are confirmed reaped, normal shutdown explicitly releases the
            // lease. SIGKILL never runs Drop: inherited daemon copies retain
            // the lock throughout their crash shutdown as before. Failed or
            // uncertain cleanup must not optimistically unlock a live service.
            let _ = self._lease.unlock();
        }
        if let Some(helper) = self.namespace_helper.take() {
            let _ = fs::remove_file(helper);
        }
    }
}

fn pin_namespace_helper(
    helper: &Path,
    directory: &Path,
    instance: &str,
    nonce: u128,
) -> Result<PathBuf, String> {
    use std::io::copy;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let pinned = directory.join(format!(".{instance}-{nonce:x}-probes-helper"));
    let mut source = File::open(helper).map_err(|e| {
        format!(
            "cannot open the Perfetto probe helper {}: {e}",
            helper.display()
        )
    })?;
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&pinned)
        .map_err(|e| {
            format!(
                "cannot pin the Perfetto probe helper {}: {e}",
                pinned.display()
            )
        })?;
    if let Err(error) = copy(&mut source, &mut destination) {
        let _ = fs::remove_file(&pinned);
        return Err(format!(
            "cannot copy the Perfetto probe helper to {}: {error}",
            pinned.display()
        ));
    }
    if let Err(error) = fs::set_permissions(&pinned, fs::Permissions::from_mode(0o700)) {
        let _ = fs::remove_file(&pinned);
        return Err(format!(
            "cannot secure the pinned Perfetto probe helper {}: {error}",
            pinned.display()
        ));
    }
    Ok(pinned)
}

impl Daemons {
    fn start(&mut self, mut command: Command, log_path: &Path) -> Result<(), String> {
        use std::io::{Read, Write};
        use std::os::unix::fs::OpenOptionsExt;
        let mut log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(log_path)
            .map_err(|e| e.to_string())?;
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start manual Perfetto daemon: {e}"))?;
        let mut stderr = child.stderr.take().expect("piped daemon stderr");
        let reader = std::thread::Builder::new()
            .name("asb-perfetto-log".into())
            .spawn(move || {
                let mut remaining = 256 * 1024;
                let mut buffer = [0u8; 4096];
                while let Ok(count) = stderr.read(&mut buffer) {
                    if count == 0 {
                        break;
                    }
                    let retained = count.min(remaining);
                    let _ = log.write_all(&buffer[..retained]);
                    remaining -= retained;
                    // Continue draining excess bytes so a noisy daemon cannot
                    // block on a full pipe after the bounded log is complete.
                }
            });
        match reader {
            Ok(reader) => self.readers.push(reader),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("cannot start Perfetto bounded log reader: {error}"));
            }
        }
        match self.children.lock() {
            Ok(mut children) => children.push(child),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Perfetto child ownership failed".into());
            }
        }
        Ok(())
    }

    fn check_alive(&self) -> Result<(), String> {
        for child in self
            .children
            .lock()
            .map_err(|_| "Perfetto child ownership failed")?
            .iter_mut()
        {
            if child.try_wait().map_err(|e| e.to_string())?.is_some() {
                return Err("owned Perfetto daemon exited during startup; inspect .asb/perfetto service logs".into());
            }
        }
        Ok(())
    }
}

impl Drop for Daemons {
    fn drop(&mut self) {
        if let Ok(mut children) = self.children.lock() {
            // No sudo wrappers or detached daemons: every child remains owned.
            for child in children.iter_mut().rev() {
                if !matches!(child.try_wait(), Ok(None)) {
                    continue;
                }
                let _ = Command::new("kill")
                    .args(["-TERM", &child.id().to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            for child in children.iter_mut().rev() {
                while child.try_wait().is_ok_and(|status| status.is_none())
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

#[cfg(target_os = "linux")]
fn socket_ready(name: &str) -> bool {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixStream};
    SocketAddr::from_abstract_name(name.trim_start_matches('@'))
        .ok()
        .is_some_and(|address| UnixStream::connect_addr(&address).is_ok())
}

#[cfg(not(target_os = "linux"))]
fn socket_ready(_: &str) -> bool {
    false
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn parent_death(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    let owner = std::process::id();
    // SAFETY: the child closure only invokes async-signal-safe kernel calls;
    // no credential-changing exec is used. Check the parent-death race too.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() as u32 != owner {
                return Err(std::io::Error::other("Perfetto owner exited during spawn"));
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn parent_death(_: &mut Command) {}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn inherit_lease(command: &mut Command, lease: &File) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let fd = lease.as_raw_fd();
    // SAFETY: only async-signal-safe fcntl calls run after fork. The Service
    // retains this fd until its spawning owner thread has joined. Inheriting
    // the same flock open-file-description keeps the lease held after ASB
    // SIGKILL until BOTH daemons have actually completed shutdown.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn inherit_lease(_: &mut Command, _: &File) {}

#[cfg(all(test, target_os = "linux"))]
#[path = "../../../../tests/unit/perfetto_service.rs"]
mod coverage_tests;

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn private_service_queries_require_both_sources_and_bounded_successful_output() {
        for (script, expected) in [
            ("print('linux.ftrace linux.process_stats')", true),
            ("print('linux.ftrace')", false),
            ("print('linux.process_stats')", false),
            (
                "print('linux.ftrace linux.process_stats'); raise SystemExit(1)",
                false,
            ),
            (
                "print('linux.ftrace linux.process_stats' + 'x' * 70000)",
                false,
            ),
        ] {
            let mut query = Command::new("python3");
            query.args(["-c", script]);
            assert_eq!(producer_registered(&mut query).unwrap(), expected);
        }
    }

    const MOCK: &str = r#"#!/usr/bin/env python3
import os, selectors, signal, socket, sys, time
from pathlib import Path
def stop(*_):
    time.sleep(0.3)
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
assert '--reset-ftrace' not in sys.argv
if sys.argv[1] == 'traced':
    sel=selectors.DefaultSelector()
    for variable in ('PERFETTO_PRODUCER_SOCK_NAME','PERFETTO_CONSUMER_SOCK_NAME'):
        sock=socket.socket(socket.AF_UNIX)
        sock.bind('\0'+os.environ[variable][1:])
        sock.listen()
        sel.register(sock,selectors.EVENT_READ)
    while True:
        for key,_ in sel.select():
            connection,_=key.fileobj.accept()
            connection.close()
elif sys.argv[1] == 'traced_probes':
    sys.stderr.write('x' * 1048576)
    sys.stderr.flush()
    Path(__file__).with_name('probes-ready').write_text('ready')
    time.sleep(60)
elif sys.argv[1] == 'perfetto':
    if '--query' in sys.argv:
        if Path(__file__).with_name('probes-ready').exists():
            print('linux.ftrace linux.process_stats')
        sys.exit(0)
    time.sleep(60)
else:
    raise RuntimeError('unexpected applet')
"#;

    fn fixture(root: &Path) -> PathBuf {
        fs::create_dir_all(root).unwrap();
        let executable = root.join("tracebox-fixture");
        crate::write_test_file(root, &executable, MOCK).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        executable
    }

    fn start_fixture(root: &Path, executable: &Path) -> Arc<Service> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("slot.lease"))
            .unwrap();
        lock.try_lock().unwrap();
        Arc::new(
            Service::start(
                executable,
                "fixture-instance".into(),
                root.join("trace_marker"),
                lock,
                root,
                None,
            )
            .unwrap(),
        )
    }

    struct TestChild(Child);
    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn normal_shutdown_releases_lease_despite_an_incidental_inherited_descriptor() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let root = std::env::temp_dir().join(
            crate::workloads::TaskIdentity::local("perfetto-inherited-lease", None).attempt_id,
        );
        let executable = fixture(&root);
        let service = start_fixture(&root, &executable);
        let descriptor = service._lease.as_raw_fd();
        let metadata = service._lease.metadata().unwrap();
        // An intentionally retained copy of the same open file description is
        // deterministic evidence for the incidental fork-before-exec window.
        let mut command = Command::new("python3");
        command.args(["-c","import pathlib,sys; pathlib.Path(sys.argv[1]).write_text('ready'); sys.stdin.buffer.read(1)"])
            .arg(root.join("incidental-ready"))
            .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        inherit_lease(&mut command, &service._lease);
        let mut incidental = TestChild(command.spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(3);
        while !root.join("incidental-ready").is_file() && Instant::now() < deadline {
            assert!(incidental.0.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(root.join("incidental-ready").is_file());
        let retained =
            fs::metadata(format!("/proc/{}/fd/{descriptor}", incidental.0.id())).unwrap();
        assert_eq!(
            (retained.dev(), retained.ino()),
            (metadata.dev(), metadata.ino())
        );
        let other = OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("slot.lease"))
            .unwrap();
        assert!(
            other.try_lock().is_err(),
            "live owned daemons must retain the lease"
        );
        let pids = service
            .children
            .lock()
            .unwrap()
            .iter()
            .map(Child::id)
            .collect::<Vec<_>>();
        drop(service);
        assert!(
            pids.iter()
                .all(|pid| !Path::new(&format!("/proc/{pid}")).exists())
        );
        assert!(
            incidental.0.try_wait().unwrap().is_none(),
            "incidental child must remain alive"
        );
        other
            .try_lock()
            .expect("confirmed normal shutdown must explicitly release the lease");
        drop(incidental);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn uncertain_owner_cleanup_never_explicitly_unlocks_an_inherited_lease() {
        for poison in [false, true] {
            let root = std::env::temp_dir().join(
                crate::workloads::TaskIdentity::local("perfetto-uncertain-lease", None).attempt_id,
            );
            fs::create_dir(&root).unwrap();
            let lease = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(root.join("slot.lease"))
                .unwrap();
            lease.try_lock().unwrap();
            let incidental = lease.try_clone().unwrap();
            let children = Arc::new(Mutex::new(Vec::new()));
            let (stop, _receiver) = mpsc::channel();
            let owner = if poison {
                let shared = Arc::clone(&children);
                assert!(
                    std::thread::spawn(move || {
                        let _guard = shared.lock().unwrap();
                        panic!("fixture-only poisoned cleanup ownership");
                    })
                    .join()
                    .is_err()
                );
                None
            } else {
                Some(Owner {
                    stop,
                    thread: std::thread::spawn(|| panic!("fixture-only owner join failure")),
                })
            };
            let service = Service {
                instance: "fixture".into(),
                marker: root.join("trace_marker"),
                executable: root.join("not-started"),
                producer: String::new(),
                consumer: String::new(),
                children,
                owner,
                namespace_helper: None,
                _lease: lease,
            };
            drop(service);
            let other = OpenOptions::new()
                .read(true)
                .write(true)
                .open(root.join("slot.lease"))
                .unwrap();
            assert!(
                other.try_lock().is_err(),
                "uncertain cleanup must not clear inherited ownership"
            );
            // This fixture has no daemons: after checking the production
            // fail-closed branch, explicitly release our synthetic ownership.
            // Merely closing the clone can itself race an unrelated fork.
            incidental.unlock().unwrap();
            drop(incidental);
            other.try_lock().unwrap();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn pinned_probe_helper_survives_removal_of_the_build_executable() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("perfetto-pinned-helper", None).attempt_id);
        fs::create_dir(&root).unwrap();
        let build_executable = root.join("asb-build-output");
        crate::write_test_file(
            &root,
            &build_executable,
            "#!/bin/sh\nprintf 'pinned-helper\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&build_executable, fs::Permissions::from_mode(0o755)).unwrap();

        let pinned = pin_namespace_helper(&build_executable, &root, "fixture", 42).unwrap();
        fs::remove_file(&build_executable).unwrap();
        let output = Command::new(&pinned).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "pinned-helper\n");
        assert_eq!(
            fs::metadata(&pinned).unwrap().permissions().mode() & 0o777,
            0o700
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn creator_thread_exit_and_failed_client_do_not_kill_a_sibling_service() {
        // A parallel libtest worker can fork while this lease is open. Even
        // CLOEXEC descriptors survive in that unrelated child until exec, so
        // daemon exit alone cannot prove immediate lock release in the shared
        // process. Keep the ownership assertions in an exclusive test process.
        const ISOLATED: &str = "ASB_TEST_ISOLATED_MANUAL_SERVICE";
        if std::env::var_os(ISOLATED).is_none() {
            let temporary = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/perfetto-service-isolation");
            fs::create_dir_all(&temporary).unwrap();
            let name = format!(
                "{}::creator_thread_exit_and_failed_client_do_not_kill_a_sibling_service",
                module_path!().split_once("::").unwrap().1
            );
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", &name, "--nocapture"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("TMPDIR", temporary)
                .env(ISOLATED, "1");
            if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
                command.env("LLVM_PROFILE_FILE", profile);
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
            return;
        }
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("perfetto-service", None).attempt_id);
        let executable = fixture(&root);
        let creator_root = root.clone();
        let service = std::thread::spawn(move || start_fixture(&creator_root, &executable))
            .join()
            .unwrap();
        // The replica that created this service has now exited. PDEATHSIG
        // must be bound to the dedicated owner, not that replica thread.
        std::thread::sleep(Duration::from_millis(100));
        service.check_alive().unwrap();
        let sibling = Arc::clone(&service);
        let mut first = TestChild(service.command("perfetto").spawn().unwrap());
        let mut second = TestChild(sibling.command("perfetto").spawn().unwrap());
        first.0.kill().unwrap();
        first.0.wait().unwrap();
        drop(service);
        sibling.check_alive().unwrap();
        assert!(second.0.try_wait().unwrap().is_none());
        let other_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("slot.lease"))
            .unwrap();
        assert!(other_lock.try_lock().is_err());
        let deadline = Instant::now() + Duration::from_secs(3);
        while !root.join("probes-ready").is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            root.join("probes-ready").is_file(),
            "bounded log reader stopped draining"
        );
        let pids = sibling
            .children
            .lock()
            .unwrap()
            .iter()
            .map(Child::id)
            .collect::<Vec<_>>();
        second.0.kill().unwrap();
        second.0.wait().unwrap();
        drop(sibling);
        assert!(
            pids.iter()
                .all(|pid| !Path::new(&format!("/proc/{pid}")).exists())
        );
        other_lock.try_lock().unwrap();
        for entry in fs::read_dir(&root)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".log"))
        {
            assert!(entry.metadata().unwrap().len() <= 256 * 1024);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cloexec_lease_remains_in_an_owned_fork_child_until_exec() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/perfetto-fork-lease")
            .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let source = r#"
import os,fcntl,sys
from pathlib import Path
path=Path(sys.argv[1])/'lease'
original=path.open('w+b')
fcntl.flock(original,fcntl.LOCK_EX|fcntl.LOCK_NB)
assert fcntl.fcntl(original,fcntl.F_GETFD)&fcntl.FD_CLOEXEC
probe=path.open('r+b')
read_fd,write_fd=os.pipe()
pid=os.fork()
if pid==0:
    os.close(write_fd)
    os.read(read_fd,1)
    os.execv('/bin/true',['true'])
os.close(read_fd)
original.close()
blocked=False
try:
    fcntl.flock(probe,fcntl.LOCK_EX|fcntl.LOCK_NB)
except BlockingIOError:
    blocked=True
finally:
    os.write(write_fd,b'x')
    os.close(write_fd)
    _,status=os.waitpid(pid,0)
assert os.waitstatus_to_exitcode(status)==0
assert blocked,'CLOEXEC does not close a lease before exec'
fcntl.flock(probe,fcntl.LOCK_EX|fcntl.LOCK_NB)
probe.close()
"#;
        let output = Command::new("/usr/bin/python3")
            .args(["-I", "-S", "-B", "-c", source])
            .arg(&root)
            .env_clear()
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "internal subprocess fixture invoked by the owner-loss regression"]
    fn owner_loss_child_fixture() {
        let Some(root) = std::env::var_os("ASB_TEST_PERFETTO_OWNER_LOSS_FIXTURE") else {
            return;
        };
        let root = PathBuf::from(root);
        let service = start_fixture(&root, &root.join("tracebox-fixture"));
        let pids = service
            .children
            .lock()
            .unwrap()
            .iter()
            .map(|child| child.id().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(root.join("daemon-pids"), pids).unwrap();
        std::thread::sleep(Duration::from_secs(60));
        drop(service);
    }

    #[test]
    fn killing_the_owner_process_terminates_manual_daemons() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("perfetto-owner-loss", None).attempt_id);
        fixture(&root);
        let mut owner = TestChild(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "collectors::adapters::perfetto::service::tests::owner_loss_child_fixture",
                    "--ignored",
                    "--nocapture",
                ])
                .env("ASB_TEST_PERFETTO_OWNER_LOSS_FIXTURE", &root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.join("daemon-pids").is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        let pids = fs::read_to_string(root.join("daemon-pids")).expect("fixture startup");
        owner.0.kill().unwrap();
        owner.0.wait().unwrap();
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("slot.lease"))
            .unwrap();
        assert!(
            lease.try_lock().is_err(),
            "crashed owner's daemon cleanup lost its inherited lease"
        );
        let running = || {
            pids.lines().any(|pid| {
                fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .is_some_and(|stat| {
                        stat.rsplit_once(") ").is_some_and(|(_, tail)| {
                            !tail.starts_with('Z') && !tail.starts_with('X')
                        })
                    })
            })
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(!running(), "a manual daemon survived owner process death");
        lease
            .try_lock()
            .expect("lease remained locked after daemon shutdown");
        fs::remove_dir_all(root).unwrap();
    }
}
