//! Private registry, descriptor and daemon fixtures; no kernel tracing service.
use super::*;
use std::os::unix::fs::PermissionsExt;
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/perfetto-service-fixtures")
            .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
        fs::create_dir_all(root.join(".asb/perfetto")).unwrap();
        let root = root.canonicalize().unwrap();
        Self(root)
    }
    fn configuration(&self, instances: Vec<String>) {
        let config = crate::project::ProjectConfig {
            perfetto_instances: instances,
            ..Default::default()
        };
        fs::write(
            self.0.join(".asb/project.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
    }
    fn service(&self, children: Arc<Mutex<Vec<Child>>>, lease: File) -> Service {
        Service {
            instance: "fixture".into(),
            marker: self.0.join("marker"),
            executable: self.0.join("must-not-execute"),
            producer: "@fixture-producer".into(),
            consumer: "@fixture-consumer".into(),
            children,
            owner: None,
            namespace_helper: None,
            _lease: lease,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
#[test]
fn admission_rejects_missing_foreign_locked_and_symlinked_instances_without_tracing() {
    // Lease release is meaningful only without unrelated fork-before-exec
    // inheritors from parallel libtest workers (see the kernel regression).
    const ISOLATED: &str = "ASB_TEST_ISOLATED_SERVICE_ADMISSION";
    if std::env::var_os(ISOLATED).is_none() {
        let name = format!(
            "{}::admission_rejects_missing_foreign_locked_and_symlinked_instances_without_tracing",
            module_path!().split_once("::").unwrap().1
        );
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", &name, "--nocapture"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
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
    let fixture = Fixture::new();
    let executable = fixture.0.join("never-executed");
    fixture.configuration(vec![]);
    assert!(
        Service::acquire(&executable, &fixture.0)
            .err()
            .unwrap()
            .contains("needs delegated")
    );
    fixture.configuration(vec!["another-project-instance".into()]);
    assert!(
        Service::acquire(&executable, &fixture.0)
            .err()
            .unwrap()
            .contains("does not belong to this project")
    );
    let instances = super::super::project::instance_names(&fixture.0).unwrap();
    fixture.configuration(instances.clone());
    let leases = instances
        .iter()
        .map(|instance| {
            let lease = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(
                    fixture
                        .0
                        .join(".asb/perfetto")
                        .join(format!("{instance}.lease")),
                )
                .unwrap();
            lease.try_lock().unwrap();
            lease
        })
        .collect::<Vec<_>>();
    assert!(
        Service::acquire(&executable, &fixture.0)
            .err()
            .unwrap()
            .contains("all of this project's Perfetto instances are leased")
    );
    drop(leases);
    for instance in &instances {
        for base in [
            "/sys/kernel/tracing/instances",
            "/sys/kernel/debug/tracing/instances",
        ] {
            assert!(
                !Path::new(base).join(instance).exists(),
                "fixture must never select an actual host instance"
            );
        }
    }
    let error = Service::acquire(&executable, &fixture.0).err().unwrap();
    assert!(error.contains("delegation is incomplete"), "{error}");
    let lease = fixture
        .0
        .join(".asb/perfetto")
        .join(format!("{}.lease", instances[0]));
    fs::remove_file(&lease).unwrap();
    let target = fixture.0.join("untouched");
    fs::write(&target, "preserved").unwrap();
    std::os::unix::fs::symlink(&target, &lease).unwrap();
    assert!(
        Service::acquire(&executable, &fixture.0)
            .err()
            .unwrap()
            .contains("cannot open Perfetto instance lease")
    );
    assert_eq!(fs::read_to_string(target).unwrap(), "preserved");
}
#[test]
fn cached_registry_service_is_reused_only_with_identical_executable() {
    let fixture = Fixture::new();
    fixture.configuration(super::super::project::instance_names(&fixture.0).unwrap());
    let children = Arc::new(Mutex::new(Vec::new()));
    let lease = File::create(fixture.0.join("cached.lease")).unwrap();
    let service = Arc::new(fixture.service(children, lease));
    let registry = SERVICES.get_or_init(|| Mutex::new(BTreeMap::new()));
    registry
        .lock()
        .unwrap()
        .insert(fixture.0.clone(), Arc::downgrade(&service));
    let reused = Service::acquire(&service.executable, &fixture.0).unwrap();
    assert!(Arc::ptr_eq(&reused, &service));
    let error = Service::acquire(&fixture.0.join("changed-executable"), &fixture.0)
        .err()
        .unwrap();
    assert!(error.contains("selected different executables"));
    registry.lock().unwrap().remove(&fixture.0);
}
#[test]
fn daemon_logs_are_bounded_drained_private_and_not_overwritten() {
    let fixture = Fixture::new();
    let children = Arc::new(Mutex::new(Vec::new()));
    let mut daemons = Daemons {
        children: Arc::clone(&children),
        readers: Vec::new(),
    };
    let log = fixture.0.join("noisy.log");
    let mut command = Command::new("python3");
    command.args([
        "-I",
        "-S",
        "-B",
        "-c",
        "import signal,sys; signal.alarm(5); sys.stderr.buffer.write(b'x'*(300*1024)); sys.stderr.flush()",
    ]);
    daemons.start(command, &log).unwrap();
    children.lock().unwrap()[0].wait().unwrap();
    assert!(
        daemons
            .check_alive()
            .unwrap_err()
            .contains("exited during startup")
    );
    drop(daemons);
    assert_eq!(fs::metadata(&log).unwrap().len(), 256 * 1024);
    assert_eq!(
        fs::metadata(&log).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut daemons = Daemons {
        children: Arc::new(Mutex::new(Vec::new())),
        readers: Vec::new(),
    };
    assert!(daemons.start(Command::new("/bin/true"), &log).is_err());
    assert_eq!(fs::metadata(&log).unwrap().len(), 256 * 1024);
    assert!(
        daemons
            .start(
                Command::new("/nonexistent-asb-owned-daemon"),
                &fixture.0.join("missing.log")
            )
            .unwrap_err()
            .contains("cannot start manual Perfetto daemon")
    );
}
#[test]
fn daemon_identity_checks_require_inherited_owned_lease_and_a_live_child() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    for inherited in [false, true] {
        let fixture = Fixture::new();
        let lease = File::create(fixture.0.join("identity.lease")).unwrap();
        let children = Arc::new(Mutex::new(Vec::new()));
        let cleanup = Daemons {
            children: Arc::clone(&children),
            readers: Vec::new(),
        };
        let observation = fixture.0.join("post-exec-lease");
        let expected = lease.metadata().unwrap();
        let mut command = Command::new("python3");
        command
            .args([
                "-I",
                "-S",
                "-B",
                "-c",
                r#"import json,os,signal,sys,time
from pathlib import Path
signal.alarm(30)
identity=None
try:
    actual=os.fstat(int(sys.argv[1]))
    identity=[actual.st_dev,actual.st_ino]
    retained=(actual.st_dev,actual.st_ino)==(int(sys.argv[2]),int(sys.argv[3]))
except OSError:
    retained=False
receipt=Path(sys.argv[4])
pending=receipt.with_name(receipt.name+'.pending')
pending.write_text(json.dumps({'pid':os.getpid(),'identity':identity,'retained':retained}))
pending.replace(receipt)
sys.stdin.buffer.read(1)
"#,
            ])
            .arg(lease.as_raw_fd().to_string())
            .arg(expected.dev().to_string())
            .arg(expected.ino().to_string())
            .arg(&observation)
            .stdin(Stdio::piped());
        if inherited {
            inherit_lease(&mut command, &lease);
        }
        children.lock().unwrap().push(command.spawn().unwrap());
        let service = fixture.service(Arc::clone(&children), lease);
        // Observe the executable's own descriptor table before asserting its
        // post-exec lease contract. Process creation alone is not our readiness
        // protocol, and this test must not inspect a transient exec transition.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !observation.exists() {
            assert!(
                children.lock().unwrap()[0].try_wait().unwrap().is_none(),
                "lease fixture exited before its post-exec observation"
            );
            assert!(
                Instant::now() < deadline,
                "lease fixture did not become ready"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let receipt: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(observation).unwrap()).unwrap();
        assert_eq!(receipt["pid"], children.lock().unwrap()[0].id());
        assert_eq!(receipt["retained"], inherited);
        assert_eq!(
            receipt["identity"] == serde_json::json!([expected.dev(), expected.ino()]),
            inherited
        );
        let result = service.check_alive();
        if inherited {
            result.unwrap();
        } else {
            assert!(result.unwrap_err().contains("instance lease"));
        }
        children.lock().unwrap()[0].kill().unwrap();
        children.lock().unwrap()[0].wait().unwrap();
        assert!(service.check_alive().unwrap_err().contains("daemon exited"));
        drop(cleanup);
    }
}
#[test]
fn abstract_socket_readiness_is_scoped_to_the_owned_listener() {
    // CLOEXEC listeners remain inherited in unrelated parallel-test children
    // between fork and exec. Check immediate last-close semantics in an exact
    // libtest child, not a process concurrently spawning unrelated children.
    const ISOLATED: &str = "ASB_TEST_ISOLATED_ABSTRACT_SOCKET";
    if std::env::var_os(ISOLATED).is_none() {
        let name = format!(
            "{}::abstract_socket_readiness_is_scoped_to_the_owned_listener",
            module_path!().split_once("::").unwrap().1
        );
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", &name, "--nocapture"])
            .env_clear()
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
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};
    let name = format!(
        "asb-fixture-{}-{}",
        std::process::id(),
        crate::workloads::TaskIdentity::local("socket", None).attempt_id
    );
    assert!(!socket_ready(&format!("@{name}")));
    let address = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let listener = UnixListener::bind_addr(&address).unwrap();
    assert!(socket_ready(&format!("@{name}")));
    listener.accept().unwrap();
    drop(listener);
    assert!(!socket_ready(&format!("@{name}")));
    assert!(!socket_ready(&"x".repeat(200)));
}

#[test]
fn cloexec_abstract_listener_survives_in_owned_child_only_until_exec() {
    // Deterministic kernel-contract witness: a pipe holds only our child
    // before exec; there are no timing sleeps, retry loops, or foreign PIDs.
    let source = r#"
import os,fcntl,socket
listener=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
address='\0asb-fork-socket-'+str(os.getpid())
listener.bind(address)
listener.listen(4)
assert fcntl.fcntl(listener,fcntl.F_GETFD)&fcntl.FD_CLOEXEC
read_fd,write_fd=os.pipe()
pid=os.fork()
if pid==0:
    os.close(write_fd)
    os.read(read_fd,1)
    os.execv('/bin/true',['true'])
os.close(read_fd)
listener.close()
try:
    probe=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
    probe.connect(address)
    probe.close()
finally:
    os.write(write_fd,b'x')
    os.close(write_fd)
    _,status=os.waitpid(pid,0)
assert os.waitstatus_to_exitcode(status)==0
probe=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
try:
    probe.connect(address)
except ConnectionRefusedError:
    pass
else:
    raise AssertionError('abstract listener survived its final exec-time close')
finally:
    probe.close()
"#;
    let output = Command::new("/usr/bin/python3")
        .args(["-I", "-S", "-B", "-c", source])
        .env_clear()
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
