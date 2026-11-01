//! Linux identity-pinned direct-child fixtures; no live provider or host state.
#![allow(unsafe_code)]
use super::*;
use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};

// Keep the historical generic custody cases on their original call shape,
// but exercise the NEW typed production entries and an actual reservation.
impl Local {
    fn spawn(&mut self, command: &mut Command, key: bool) -> Result<String, String> {
        if key {
            self.spawn_bridge(command)
        } else {
            let command = std::mem::replace(command, Command::new("unused-test-command"));
            self.spawn_proxy(command).map(|_| String::new())
        }
    }
}

thread_local! {
    static READER_FAIL: Cell<bool> = const { Cell::new(false) };
    static AFTER_SPAWN: Cell<u8> = const { Cell::new(0) };
    static POLL_FAIL: Cell<u8> = const { Cell::new(0) };
    static IDENTITIES: RefCell<Vec<(u32, File)>> = const { RefCell::new(Vec::new()) };
    static READERS: RefCell<Vec<Arc<AtomicBool>>> = const { RefCell::new(Vec::new()) };
}

pub(super) fn reader_receipt() -> Arc<AtomicBool> {
    let receipt = Arc::new(AtomicBool::new(false));
    READERS.with_borrow_mut(|readers| readers.push(Arc::clone(&receipt)));
    receipt
}
fn readers_finished() {
    let deadline = Instant::now() + Duration::from_secs(3);
    READERS.with_borrow_mut(|readers| {
        for receipt in readers.drain(..) {
            while !receipt.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "stopped output worker did not exit"
                );
                std::thread::sleep(POLL);
            }
        }
    });
}

pub(super) fn reader_creation() -> Result<(), String> {
    if READER_FAIL.get() {
        Err("fixture reader creation failure".into())
    } else {
        Ok(())
    }
}
pub(super) fn after_spawn(pid: u32) -> Result<(), String> {
    if AFTER_SPAWN.get() != 0 {
        // SAFETY: pin the directly owned child before any cleanup or handoff.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        assert!(fd >= 0);
        if AFTER_SPAWN.get() == 3 {
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: wait only for the pinned fixture to exit, without reaping.
            assert_eq!(unsafe { libc::poll(&mut poll, 1, 2000) }, 1);
        }
        IDENTITIES.with_borrow_mut(|ids| ids.push((pid, unsafe { File::from_raw_fd(fd) })));
    }
    match AFTER_SPAWN.get() {
        1 => Err("fixture primary startup failure".into()),
        2 => panic!("fixture startup unwind"),
        _ => Ok(()),
    }
}
pub(super) fn cleanup_timeout() -> Option<Duration> {
    (POLL_FAIL.get() != 0).then_some(Duration::ZERO)
}
pub(super) fn poll_override() -> Option<std::io::Result<Option<std::process::ExitStatus>>> {
    match POLL_FAIL.get() {
        1 => Some(Ok(None)),
        2 => Some(Err(std::io::Error::other("fixture poll failure"))),
        _ => None,
    }
}

fn command(script: &str) -> Command {
    let python = crate::tools::host_executable(std::path::Path::new("python3")).unwrap();
    let mut command = Command::new(python);
    command
        .env_clear()
        .args(["-u", "-c", script])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn assert_original_socket_released(identity: (u64, u64)) {
    use std::os::unix::fs::MetadataExt;
    for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
        if let Ok(metadata) = std::fs::metadata(entry.unwrap().path()) {
            assert_ne!(
                (metadata.dev(), metadata.ino()),
                identity,
                "original socket retained in parent FD custody"
            );
        }
    }
    // Allow bounded fork/exec transients to close CLOEXEC copies; numeric
    // ephemeral-port reuse by parallel actors is not an ownership failure.
    let end = Instant::now() + Duration::from_secs(3);
    let inode = identity.1.to_string();
    loop {
        let present = std::fs::read_to_string("/proc/self/net/tcp")
            .unwrap()
            .lines()
            .any(|line| line.split_whitespace().nth(9) == Some(inode.as_str()));
        if !present {
            break;
        }
        assert!(
            Instant::now() < end,
            "original listening socket inode {} remained after confirmed cleanup",
            identity.1
        );
        std::thread::sleep(POLL);
    }
}

#[test]
fn output_limits_and_post_readiness_drain_are_bounded() {
    for (script, expected) in [
        (
            "import sys,time;sys.stdout.write('x'*8192);sys.stdout.flush();time.sleep(30)",
            "byte or line limit",
        ),
        (
            "import time;print('x'*8192,flush=True);time.sleep(30)",
            "byte or line limit",
        ),
        (
            "import time;print(('x'*2048+'\\n')*40,flush=True);time.sleep(30)",
            "byte or line limit",
        ),
        (
            "import time;print('noise\\n'*64,flush=True);time.sleep(30)",
            "message limit",
        ),
        ("import time;time.sleep(30)", "within 5 seconds"),
    ] {
        let mut local = Local::new().unwrap();
        let start = Instant::now();
        let error = local.spawn(&mut command(script), true).unwrap_err();
        let pid = local.children[0].id();
        assert!(error.contains(expected), "{error}");
        drop(local);
        assert!(start.elapsed() < Duration::from_secs(7));
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
    let mut local = Local::new().unwrap();
    assert_eq!(local.spawn(&mut command("import sys,time;print('Codex Bridge API key: fixture-secret',flush=True);sys.stdout.write('x'*1000000);sys.stdout.flush();time.sleep(30)"), true).unwrap(), "fixture-secret");
    std::thread::sleep(Duration::from_millis(100));
    assert!(local.children[0].try_wait().unwrap().is_none());
    assert!(!local.readers[0].worker.as_ref().unwrap().is_finished());
    drop(local);
    readers_finished();
}

#[test]
fn fallible_reader_and_reaper_precede_every_child() {
    let scope = crate::process_cleanup::test_support::Scope::new();
    crate::process_cleanup::test_support::set_creation_failure(true);
    assert!(Local::new().is_err());
    crate::process_cleanup::test_support::set_creation_failure(false);
    let mut local = Local::new().unwrap();
    READER_FAIL.set(true);
    assert!(
        local
            .spawn(&mut command("raise Exception('must not start')"), true)
            .is_err()
    );
    READER_FAIL.set(false);
    assert!(local.children.is_empty());
    local.spawn(&mut command("import time;print('Codex Bridge API key: fixture-secret',flush=True);time.sleep(30)"), true).unwrap();
    READER_FAIL.set(true);
    assert!(
        local
            .spawn(&mut command("raise Exception('must not start')"), false)
            .is_err()
    );
    READER_FAIL.set(false);
    assert_eq!(local.children.len(), 1);
    drop(local);
    scope.wait_finished();
    readers_finished();
}

#[test]
fn cleanup_stops_reader_even_when_parent_retains_an_open_output_writer() {
    let mut local = Local::new().unwrap();
    let (mut input, writer) = UnixStream::pair().unwrap();
    input.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let (sender, _receiver) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || drain_output(&mut input, &stopped, &sender, true));
    local.readers.push(Reader {
        stop,
        worker: Some(worker),
        proxy: false,
    });
    // A retained writer prevents EOF, but cooperative cancellation still exits.
    assert!(local.cleanup().is_ok());
    assert!(local.readers.is_empty());
    drop(writer);
}

#[test]
fn ready_marker_cannot_admit_an_already_exited_child() {
    for (script, key) in [
        (
            "print('Codex Bridge API key: fixture-secret',flush=True)",
            true,
        ),
        (
            "import socket,sys,json;port=socket.socket(fileno=int(sys.argv[sys.argv.index('--listen-fd')+1])).getsockname()[1];print('ASB cli2key proxy ready '+json.dumps({'schema_version':1,'host':'127.0.0.1','port':port}),flush=True)",
            false,
        ),
    ] {
        let scope = crate::process_cleanup::test_support::Scope::new();
        let mut local = Local::new().unwrap();
        AFTER_SPAWN.set(3);
        let error = local.spawn(&mut command(script), key).unwrap_err();
        AFTER_SPAWN.set(0);
        let cleanup = local.cleanup();
        drop(local);
        scope.wait_finished();
        readers_finished();
        IDENTITIES.with_borrow_mut(|identities| {
            assert_eq!(identities.len(), 1);
            for (_, identity) in identities.drain(..) {
                let mut exited = libc::pollfd {
                    fd: identity.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: observe only the pinned original child's identity.
                assert_eq!(unsafe { libc::poll(&mut exited, 1, 0) }, 1);
            }
        });
        assert!(cleanup.is_ok(), "{cleanup:?}");
        let label = if key {
            "cli2key bridge"
        } else {
            "cli2key compatibility proxy"
        };
        // AFTER_SPAWN3 already proved exit. Reader-ready versus exit polling
        // may win first, but both exact paths must reject the dead producer.
        assert!(
            error == format!("{label} exited at readiness with exit status: 0")
                || error
                    == format!(
                        "{label} closed its startup output with exit status: 0; {label} did not become ready"
                    ),
            "{error}"
        );
        assert!(!error.contains("fixture-secret"));
    }
}

#[test]
fn final_admission_rechecks_bridge_after_independent_proxy_readiness() {
    let scope = crate::process_cleanup::test_support::Scope::new();
    let mut sentinel = command("import time;time.sleep(30)")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut local = Local::new().unwrap();
    let mut bridge = command(
        "import sys;print('Codex Bridge API key: fixture-secret',flush=True);sys.stdin.buffer.read(1)",
    );
    bridge.stdin(Stdio::piped());
    local.spawn(&mut bridge, true).unwrap();
    let first_pid = local.children[0].id();
    // SAFETY: pin the owned bridge before permitting its controlled exit.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, first_pid, 0) } as i32;
    assert!(fd >= 0);
    let pinned = unsafe { File::from_raw_fd(fd) };
    local.children[0]
        .stdin
        .take()
        .unwrap()
        .write_all(b"x")
        .unwrap();
    let mut exited = libc::pollfd {
        fd: pinned.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: deterministically observe exit without reaping or PID guesses.
    assert_eq!(unsafe { libc::poll(&mut exited, 1, 2000) }, 1);
    local
        .spawn(
            &mut command("import socket,sys,json,time;listener=socket.socket(fileno=int(sys.argv[sys.argv.index('--listen-fd')+1]));print('ASB cli2key proxy ready '+json.dumps({'schema_version':1,'host':'127.0.0.1','port':listener.getsockname()[1]}),flush=True);time.sleep(30)"),
            false,
        )
        .unwrap();
    let second_pid = local.children[1].id();
    let error = local.confirm_ready().unwrap_err();
    assert!(
        error.starts_with("cli2key bridge exited before session admission"),
        "{error}"
    );
    assert!(!error.contains("fixture-secret"));
    assert_eq!(local.failure(error.clone()), error);
    scope.wait_finished();
    readers_finished();
    for pid in [first_pid, second_pid] {
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
    assert!(sentinel.try_wait().unwrap().is_none());
    sentinel.kill().unwrap();
    sentinel.wait().unwrap();
}

#[test]
fn ignored_lifetime_eof_uses_grace_inside_the_shared_cleanup_budget() {
    let mut local = Local::new().unwrap();
    let mut proxy = command(
        "import socket,sys,json,time;listener=socket.socket(fileno=int(sys.argv[sys.argv.index('--listen-fd')+1]));print('ASB cli2key proxy ready '+json.dumps({'schema_version':1,'host':'127.0.0.1','port':listener.getsockname()[1]}),flush=True);time.sleep(30)",
    );
    proxy.stdin(Stdio::piped());
    local.spawn(&mut proxy, false).unwrap();
    let pid = local.children[0].id();
    let started = Instant::now();
    let error = local.finish().unwrap_err();
    assert!(
        error.contains("cooperative shutdown before its grace deadline"),
        "{error}"
    );
    assert!(started.elapsed() >= Duration::from_millis(1800));
    assert!(started.elapsed() < Duration::from_millis(4500));
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    readers_finished();
}

#[test]
fn embedded_proxy_finish_cancels_and_reaps_its_owned_curl_before_exit() {
    embedded_proxy_finish_fixture(false);
}

#[test]
fn high_descriptor_listener_must_not_escape_to_the_owned_curl() {
    embedded_proxy_finish_fixture(true);
}

#[test]
fn explicit_runtime_launcher_is_preserved_for_high_fd_exec_handoff() {
    const CHILD: &str = "ASB_OWNED_EXEC_LAUNCHER_FIXTURE";
    if std::env::var_os(CHILD).is_some() {
        embedded_proxy_finish_fixture(true);
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/cli2key-launcher")
        .join(crate::workloads::TaskIdentity::local("launcher", None).attempt_id);
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let actual = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    let launcher = root.join("asb-launcher");
    let marker = root.join("launcher-used");
    crate::write_test_file(
        &root,
        &launcher,
        format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$$\" \"$1\" >> '{}'\nexec '{}' \"$@\"\n",
            marker.display(),
            actual.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let name = format!(
        "{}::explicit_runtime_launcher_is_preserved_for_high_fd_exec_handoff",
        module_path!().split_once("::").unwrap().1
    );
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", &root)
        .env(CHILD, &marker)
        .env("ASB_RUNTIME_EXECUTABLE", &launcher)
        .args(["--exact", &name, "--nocapture"]);
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!std::fs::read_to_string(marker).unwrap().is_empty());
}

fn embedded_proxy_finish_fixture(high_descriptor: bool) {
    use std::net::{TcpListener, TcpStream};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/cli2key-custody-fixtures")
        .join(crate::workloads::TaskIdentity::local("proxy", None).attempt_id);
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join("proxy.mjs"), super::super::BUILTIN_COMPAT_PROXY).unwrap();
    std::fs::write(
        root.join("cli2key-chat-compat.mjs"),
        super::super::CHAT_COMPAT,
    )
    .unwrap();
    std::fs::write(root.join("network-fetch.mjs"), super::super::NETWORK_FETCH).unwrap();
    std::fs::write(root.join("observe.mjs"), r#"import http from 'node:http';
import childProcess from 'node:child_process';
import {syncBuiltinESMExports} from 'node:module';
import {writeFileSync,appendFileSync} from 'node:fs';
process.on('exit',code=>writeFileSync('exit-status',String(code)));
const spawn=childProcess.spawn;
childProcess.spawn=function(program,args,...options){
  const child=spawn.call(this,program,args,...options);
  appendFileSync('spawn-observed.jsonl',JSON.stringify({program,mode:args[0],pid:child.pid})+'\n',{mode:0o600});
  return child;
};
syncBuiltinESMExports();
const create=http.createServer;
http.createServer=function(...args){
  const server=create.apply(this,args);
  server.on('request',request=>{
    if(request.url==='/v1/abort'||request.url==='/v1/unfinished'){
      const name=request.url==='/v1/abort'?'abort':'unfinished';
      writeFileSync(name+'-seen','yes');
      request.once('aborted',()=>writeFileSync(name+'-aborted','yes'));
    }
  });
  return server;
};
"#).unwrap();
    let node = crate::tools::host_executable(std::path::Path::new("node")).unwrap();
    let curl = crate::tools::host_executable(std::path::Path::new("curl")).unwrap();
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    held.set_nonblocking(true).unwrap();
    let mut proxy = Command::new(node);
    proxy
        .env_clear()
        .current_dir(&root)
        .env("PATH", curl.parent().unwrap())
        .env(
            "http_proxy",
            format!("http://{}", held.local_addr().unwrap()),
        )
        .env("NO_PROXY", "")
        .args([
            "--import",
            "./observe.mjs",
            "proxy.mjs",
            "--upstream",
            "http://127.0.0.1:1/v1",
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::null());
    let mut local = Local::new().unwrap();
    let address = if high_descriptor {
        // Own and release these inert descriptors only; leave a hole below a
        // high listener FD before exec. No shared flags or foreign FDs change.
        let held: Vec<_> = (0..256).map(|_| File::open("/dev/null").unwrap()).collect();
        let reservation = super::super::proxy::Reservation::new().unwrap();
        drop(held);
        local.spawn_reserved_proxy(proxy, reservation).unwrap()
    } else {
        local.spawn_proxy(proxy).unwrap()
    };
    let port = address.port();
    let original_socket = local.reservation.as_ref().unwrap().socket_identity();
    let proxy_pid = local.children[0].id();
    let mut request = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
    request
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    request.write_all(b"GET /v1/held HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer fixture-only-key\r\nConnection: close\r\n\r\n").unwrap();
    let mut first_upstream = accept_held_request(&held);
    let mut aborted = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
    aborted
        .write_all(
            b"POST /v1/abort HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10000\r\n\r\npartial",
        )
        .unwrap();
    wait_fixture_receipt(&root.join("abort-seen"));
    aborted.shutdown(std::net::Shutdown::Both).unwrap();
    drop(aborted);
    wait_fixture_receipt(&root.join("abort-aborted"));
    // The sibling request must complete even after an ordinary body abort.
    first_upstream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nlive")
        .unwrap();
    request
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut response = String::new();
    request.read_to_string(&mut response).unwrap();
    assert!(
        response.contains("live"),
        "ordinary client abort crashed its sibling: {response}"
    );
    drop(first_upstream);
    let mut request = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
    request
        .write_all(
            b"GET /v1/held-shutdown HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    let mut upstream = accept_held_request(&held);
    let mut unfinished = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
    unfinished.write_all(b"POST /v1/unfinished HTTP/1.1\r\nHost: localhost\r\nContent-Length: 10000\r\n\r\npartial").unwrap();
    wait_fixture_receipt(&root.join("unfinished-seen"));
    let mut chunk = [0; 1024];
    let children =
        std::fs::read_to_string(format!("/proc/{proxy_pid}/task/{proxy_pid}/children")).unwrap();
    let pids = children
        .split_whitespace()
        .map(|pid| pid.parse::<u32>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(pids.len(), 1, "expected the hop's one directly owned curl");
    let curl_pid = pids[0];
    let listener_inode = std::fs::read_to_string("/proc/self/net/tcp")
        .unwrap()
        .lines()
        .find_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            (fields.get(1) == Some(&format!("0100007F:{port:04X}").as_str())
                && fields.get(3) == Some(&"0A"))
            .then(|| fields[9].to_owned())
        })
        .expect("owned listening socket not found");
    let listener_target = format!("socket:[{listener_inode}]");
    let mut inherited_listener = false;
    for entry in std::fs::read_dir(format!("/proc/{curl_pid}/fd")).unwrap() {
        if let Ok(target) = std::fs::read_link(entry.unwrap().path()) {
            inherited_listener |= target.to_string_lossy() == listener_target;
        }
    }
    let parent_listener_fds: Vec<_> = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            (std::fs::read_link(entry.path()).ok()?.to_string_lossy() == listener_target)
                .then(|| entry.file_name())
        })
        .collect();
    println!(
        "high_descriptor={high_descriptor} parent_listener_fds={parent_listener_fds:?} curl_inherited={inherited_listener}"
    );
    // SAFETY: pin only the observed direct child of the fixture-owned proxy.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, curl_pid, 0) } as i32;
    assert!(fd >= 0);
    let pinned = unsafe { File::from_raw_fd(fd) };
    local.finish().unwrap();
    assert_original_socket_released(original_socket);
    readers_finished();
    assert_eq!(
        std::fs::read_to_string(root.join("exit-status")).unwrap(),
        "0",
        "the proxy must confirm cooperative shutdown, not be killed"
    );
    assert_eq!(
        upstream.read(&mut chunk).unwrap(),
        0,
        "owned outbound transport remained open"
    );
    let mut exited = libc::pollfd {
        fd: pinned.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut exited, 1, 0) }, 1);
    for pid in [proxy_pid, curl_pid] {
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
    for connection in [&mut unfinished, &mut request] {
        connection
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        match connection.read_to_end(&mut bytes) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            Err(error) => panic!("incoming connection remained open after finish: {error}"),
        }
        assert!(
            bytes.is_empty(),
            "closing/aborted requests must not receive a late 502"
        );
    }
    drop(request);
    drop(upstream);
    drop(held);
    if let Some(marker) = std::env::var_os("ASB_OWNED_EXEC_LAUNCHER_FIXTURE") {
        let observed = std::fs::read_to_string(root.join("spawn-observed.jsonl")).unwrap();
        let launches: Vec<serde_json::Value> = observed
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!launches.is_empty());
        let expected_launcher = std::env::var("ASB_RUNTIME_EXECUTABLE").unwrap();
        let mut expected: Vec<_> = launches
            .iter()
            .map(|launch| {
                assert_eq!(
                    launch["program"], expected_launcher,
                    "an actual spawn bypassed the configured launcher"
                );
                assert_eq!(launch["mode"], "__asb-owned-fd-exec");
                format!("{}|__asb-owned-fd-exec", launch["pid"].as_u64().unwrap())
            })
            .collect();
        let text = std::fs::read_to_string(&marker).unwrap();
        let mut actual: Vec<_> = text.lines().map(str::to_owned).collect();
        actual.sort();
        expected.sort();
        assert_eq!(
            actual, expected,
            "every actual outbound child must execute through the configured launcher in the SAME PID"
        );
        std::fs::write(
            std::path::Path::new(&marker).with_extension("spawn-proof.jsonl"),
            observed,
        )
        .unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
    // Intentional high-FD reproduction asserts only AFTER confirmed proxy,
    // curl, connection and reservation cleanup above.
    assert!(
        !inherited_listener,
        "curl inherited the owned listening socket"
    );
}

fn wait_fixture_receipt(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "fixture did not observe {}",
            path.display()
        );
        std::thread::sleep(POLL);
    }
}

fn accept_held_request(listener: &std::net::TcpListener) -> std::net::TcpStream {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(POLL)
            }
            Err(error) => panic!("owned curl did not reach loopback fixture: {error}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut header = Vec::new();
    let mut chunk = [0; 1024];
    while !header.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0 && header.len() + count < 8192);
        header.extend_from_slice(&chunk[..count]);
    }
    stream
}

#[test]
fn errors_unwinds_and_uncertain_exit_retain_direct_child_custody() {
    let mut sentinel = command("import time;time.sleep(30)")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    for children in [1, 2] {
        for failure in [1, 2, 3, 4] {
            let scope = crate::process_cleanup::test_support::Scope::new();
            let mut local = Local::new().unwrap();
            let script = "import time;print('Codex Bridge API key: fixture-secret',flush=True);time.sleep(30)";
            let mut pids = Vec::new();
            if children == 2 {
                local.spawn(&mut command(script), true).unwrap();
                pids.push(local.children[0].id());
            }
            AFTER_SPAWN.set(if failure <= 2 { failure } else { 1 });
            POLL_FAIL.set(if failure >= 3 { failure - 2 } else { 0 });
            let start = Instant::now();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let error = local.spawn(&mut command(script), true).unwrap_err();
                pids.extend(local.children.iter().map(Child::id));
                let error = local.failure(error);
                assert!(error.contains("fixture primary startup failure"));
                if failure >= 3 {
                    assert!(error.contains("owned reaper retains"));
                }
            }));
            assert_eq!(result.is_err(), failure == 2);
            AFTER_SPAWN.set(0);
            POLL_FAIL.set(0);
            scope.wait_finished();
            readers_finished();
            assert!(start.elapsed() < Duration::from_secs(2));
            IDENTITIES.with_borrow_mut(|ids| {
                for (pid, pinned) in ids.drain(..) {
                    let mut poll = libc::pollfd {
                        fd: pinned.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: valid pinned descriptor and owned child identity.
                    assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 1);
                    assert_ne!(poll.revents & libc::POLLIN, 0);
                    assert_eq!(
                        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
                        -1
                    );
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ECHILD)
                    );
                }
            });
            for pid in pids {
                assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
            }
            assert!(sentinel.try_wait().unwrap().is_none());
        }
    }
    sentinel.kill().unwrap();
    sentinel.wait().unwrap();
}

#[test]
fn explicit_session_finish_is_once_reports_uncertainty_and_retains_custody() {
    use crate::providers::ProviderSession;
    for mode in [0, 1, 2] {
        let scope = crate::process_cleanup::test_support::Scope::new();
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        std::fs::create_dir_all(&root).unwrap();
        let files = super::super::RuntimeFiles::create(&root).unwrap();
        let files_path = files.path.clone();
        let mut owner = Local::new().unwrap();
        owner.spawn(&mut command("import time;print('Codex Bridge API key: fixture-secret',flush=True);time.sleep(30)"), true).unwrap();
        let mut bridge = super::super::Bridge {
            api_key: "fixture-secret".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            _session: super::super::Session::Local {
                _owner: owner,
                _files: files,
            },
        };
        POLL_FAIL.set(mode);
        let result = bridge.finish();
        POLL_FAIL.set(0);
        assert_eq!(result.is_err(), mode != 0);
        assert_eq!(files_path.exists(), mode != 0);
        if let Err(error) = result {
            assert!(error.to_string().contains("owned reaper retains"));
        }
        assert!(
            bridge.finish().is_ok(),
            "already-settled finish must not run cleanup twice"
        );
        drop(bridge);
        scope.wait_finished();
        std::fs::remove_dir_all(root).unwrap();
        readers_finished();
    }
}

#[test]
fn owned_proxy_sessions_are_parallel_distinct_and_release_every_parent_reference() {
    use std::net::{TcpListener, TcpStream};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/cli2key-owned-listeners")
        .join(crate::workloads::TaskIdentity::local("parallel", None).attempt_id);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("proxy.mjs"), super::super::BUILTIN_COMPAT_PROXY).unwrap();
    std::fs::write(
        root.join("cli2key-chat-compat.mjs"),
        super::super::CHAT_COMPAT,
    )
    .unwrap();
    std::fs::write(root.join("network-fetch.mjs"), super::super::NETWORK_FETCH).unwrap();
    let root = root.canonicalize().unwrap();
    let node = crate::tools::host_executable(std::path::Path::new("node")).unwrap();
    let sentinel = TcpListener::bind("127.0.0.1:0").unwrap();
    let sentinel_address = sentinel.local_addr().unwrap();
    let sessions = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let root = &root;
                let node = &node;
                scope.spawn(move || {
                    let mut command = Command::new(node);
                    command
                        .env_clear()
                        .current_dir(root)
                        .args(["proxy.mjs", "--upstream", "http://127.0.0.1:1/v1"])
                        .stdin(Stdio::piped())
                        .stderr(Stdio::null());
                    let mut local = Local::new().unwrap();
                    let address = local.spawn_proxy(command).unwrap();
                    local.confirm_ready().unwrap();
                    let identity = local.reservation.as_ref().unwrap().socket_identity();
                    (local, address, identity)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let addresses: std::collections::BTreeSet<_> =
        sessions.iter().map(|(_, address, _)| *address).collect();
    assert_eq!(addresses.len(), 4);
    for (local, address, identity) in sessions {
        assert!(
            TcpListener::bind(address).is_err(),
            "session released the reserved socket"
        );
        local.finish().unwrap();
        assert_original_socket_released(identity);
        assert!(
            TcpStream::connect(sentinel_address).is_ok(),
            "unrelated sentinel listener was closed"
        );
    }
}

#[test]
fn proxy_ready_faults_and_late_duplicates_reach_admission_or_finish() {
    for fault in [
        "wrong-port",
        "wrong-host",
        "zero",
        "malformed",
        "duplicate",
        "oversized",
    ] {
        let mut local = Local::new().unwrap();
        let script = format!(
            r#"import socket,sys,json,threading,os,time
listener=socket.socket(fileno=int(sys.argv[sys.argv.index('--listen-fd')+1]))
threading.Thread(target=lambda:(sys.stdin.buffer.read(),os._exit(0)),daemon=True).start()
ready={{'schema_version':1,'host':'127.0.0.1','port':listener.getsockname()[1]}}
fault={fault:?}
if fault=='wrong-port': ready['port']=1 if ready['port']!=1 else 2
if fault=='wrong-host': ready['host']='0.0.0.0'
if fault=='zero': ready['port']=0
line='ASB cli2key proxy ready '+json.dumps(ready)
if fault=='malformed': line='ASB cli2key proxy ready broken'
if fault=='oversized': line='ASB cli2key proxy ready '+'x'*5000
if fault=='duplicate': line+='\n'+line
print(line,flush=True)
time.sleep(30)
"#
        );
        let mut command = command(&script);
        command.stdin(Stdio::piped());
        let result = local.spawn_proxy(command);
        assert!(result.is_err(), "accepted {fault}");
        let pids: Vec<_> = local.children.iter().map(Child::id).collect();
        drop(local);
        for pid in pids {
            assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        }
        readers_finished();
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/cli2key-late-ready")
        .join(crate::workloads::TaskIdentity::local("late", None).attempt_id);
    std::fs::create_dir_all(&root).unwrap();
    let mut command = command(
        r#"import socket,sys,json,threading,os,time
listener=socket.socket(fileno=int(sys.argv[sys.argv.index('--listen-fd')+1]))
threading.Thread(target=lambda:(sys.stdin.buffer.read(),os._exit(0)),daemon=True).start()
line='ASB cli2key proxy ready '+json.dumps({'schema_version':1,'host':'127.0.0.1','port':listener.getsockname()[1]})
print(line,flush=True)
while not os.path.exists('release'): time.sleep(.005)
print(line,flush=True)
while True: time.sleep(1)
"#,
    );
    command.current_dir(&root).stdin(Stdio::piped());
    let mut local = Local::new().unwrap();
    local.spawn_proxy(command).unwrap();
    let original_socket = local.reservation.as_ref().unwrap().socket_identity();
    local.confirm_ready().unwrap();
    std::fs::write(root.join("release"), "release").unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    while local.proxy.as_ref().unwrap().check().is_ok() {
        assert!(Instant::now() < end);
        std::thread::sleep(POLL);
    }
    assert!(local.confirm_ready().is_err());
    assert!(local.finish().unwrap_err().contains("duplicate ready"));
    assert_original_socket_released(original_socket);
    readers_finished();
}

#[test]
fn proxy_preparation_errors_unwind_and_uncertain_handoff_release_the_reservation() {
    use std::net::{TcpListener, TcpStream};
    let sentinel = TcpListener::bind("127.0.0.1:0").unwrap();
    let sentinel_address = sentinel.local_addr().unwrap();
    for failure in ["reader", "spawn", "startup", "unwind", "handoff"] {
        let scope = crate::process_cleanup::test_support::Scope::new();
        let reservation = super::super::proxy::Reservation::new().unwrap();
        let identity = reservation.socket_identity();
        READER_FAIL.set(failure == "reader");
        AFTER_SPAWN.set(match failure {
            "unwind" => 2,
            "startup" | "handoff" => 1,
            _ => 0,
        });
        POLL_FAIL.set(u8::from(failure == "handoff"));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut local = Local::new().unwrap();
            let mut command = if failure == "spawn" {
                Command::new("/definitely-absent-offline-fixture")
            } else {
                command(
                    "import socket,sys,json,threading,os,time;listener=socket.socket(fileno=int(sys.argv[sys.argv.index('--listen-fd')+1]));threading.Thread(target=lambda:(sys.stdin.buffer.read(),os._exit(0)),daemon=True).start();print('ASB cli2key proxy ready '+json.dumps({'schema_version':1,'host':'127.0.0.1','port':listener.getsockname()[1]}),flush=True);time.sleep(30)",
                )
            };
            command.stdin(Stdio::piped());
            let error = local
                .spawn_reserved_proxy(command, reservation)
                .unwrap_err();
            let no_child = local.children.is_empty();
            let combined = local.failure(error);
            if failure == "handoff" {
                assert!(combined.contains("owned reaper retains"), "{combined}");
            }
            if failure == "reader" || failure == "spawn" {
                assert!(no_child);
                assert!(
                    !combined.contains("cleanup was not confirmed"),
                    "{combined}"
                );
            }
        }));
        READER_FAIL.set(false);
        AFTER_SPAWN.set(0);
        POLL_FAIL.set(0);
        scope.wait_finished();
        readers_finished();
        assert_eq!(result.is_err(), failure == "unwind");
        // This checks BOTH the child reference and the parent's Command
        // pre_exec closure/reference, after exact reaper settlement if needed.
        assert_original_socket_released(identity);
        IDENTITIES.with_borrow_mut(|identities| {
            for (_, identity) in identities.drain(..) {
                let mut poll = libc::pollfd {
                    fd: identity.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 1);
            }
        });
        assert!(TcpStream::connect(sentinel_address).is_ok());
    }
}

#[test]
fn builtin_proxy_rejects_invalid_or_nonsocket_fd_without_rebinding_a_foreign_listener() {
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::process::CommandExt;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/cli2key-invalid-listener")
        .join(crate::workloads::TaskIdentity::local("invalid", None).attempt_id);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("proxy.mjs"), super::super::BUILTIN_COMPAT_PROXY).unwrap();
    std::fs::write(
        root.join("cli2key-chat-compat.mjs"),
        super::super::CHAT_COMPAT,
    )
    .unwrap();
    std::fs::write(root.join("network-fetch.mjs"), super::super::NETWORK_FETCH).unwrap();
    std::fs::write(root.join("not-a-socket"), "preserve owned file bytes").unwrap();
    let root = root.canonicalize().unwrap();
    let node = crate::tools::host_executable(std::path::Path::new("node")).unwrap();
    let sentinel = TcpListener::bind("127.0.0.1:0").unwrap();
    let sentinel_address = sentinel.local_addr().unwrap();
    for nonsocket in [false, true] {
        let file = File::open(root.join("not-a-socket")).unwrap();
        let fd = file.as_raw_fd();
        let mut command = Command::new(&node);
        command
            .env_clear()
            .current_dir(&root)
            .args([
                "proxy.mjs",
                "--listen-fd",
                &if nonsocket {
                    fd.to_string()
                } else {
                    "999999".into()
                },
                "--upstream",
                &format!("http://{sentinel_address}/v1"),
            ])
            .stdin(Stdio::piped())
            .stderr(Stdio::null());
        if nonsocket {
            // SAFETY: isolated child-only inheritance of this owned regular
            // file. No parent flags, foreign path or socket are changed.
            unsafe {
                command.pre_exec(move || {
                    let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFD);
                    if flags < 0
                        || libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags & !libc::FD_CLOEXEC)
                            < 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let reservation = super::super::proxy::Reservation::new().unwrap();
        let identity = reservation.socket_identity();
        let mut local = Local::new().unwrap();
        let error = local
            .spawn_reserved_proxy(command, reservation)
            .unwrap_err();
        let pids: Vec<_> = local.children.iter().map(Child::id).collect();
        let combined = local.failure(error);
        readers_finished();
        assert!(combined.contains("did not become ready"), "{combined}");
        for pid in pids {
            assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        }
        assert_original_socket_released(identity);
        assert!(TcpStream::connect(sentinel_address).is_ok());
        assert_eq!(
            std::fs::read_to_string(root.join("not-a-socket")).unwrap(),
            "preserve owned file bytes"
        );
    }
}
