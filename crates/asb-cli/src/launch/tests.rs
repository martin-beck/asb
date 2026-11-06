#![allow(unsafe_code)] // Owned pipe and per-child Unix credential assertions.
use super::*;
use crate::process_gate::Admission;
use std::io::Read;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

#[test]
fn gate_diagnostic_program_never_echoes_arbitrary_names_or_paths() {
    assert_eq!(
        diagnostic_program(OsStr::new("/sensitive/path/python3.12")),
        "python"
    );
    assert_eq!(
        diagnostic_program(OsStr::new("/sensitive/path/docker")),
        "docker"
    );
    assert_eq!(
        diagnostic_program(OsStr::new("/sensitive/path/private-token")),
        "other"
    );
    assert_eq!(
        diagnostic_program(OsStr::from_bytes(b"/path/\xff")),
        "other"
    );
}

fn binary() -> PathBuf {
    let path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("asb");
    assert!(path.is_file(), "build current ASB before launch tests");
    path
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/launch-fixtures")
            .join(crate::workloads::TaskIdentity::local("launch", None).attempt_id);
        std::fs::create_dir_all(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = crate::fixture_writer::cleanup(&self.0);
    }
}

fn execute(launch: &ProcessLaunch, configure: impl FnOnce(&mut Command)) -> Output {
    let (mut command, route) = launch.native(Some(&binary()));
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure(&mut command);
    let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
    let mut gate = admission.spawn().unwrap();
    let mut out = gate.take_stdout();
    let mut err = gate.take_stderr();
    gate.resume().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = gate.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "owned fixture exceeded its runtime"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(out) = out.as_mut() {
        out.take(64 * 1024).read_to_end(&mut stdout).unwrap();
    }
    if let Some(err) = err.as_mut() {
        err.take(64 * 1024).read_to_end(&mut stderr).unwrap();
    }
    Output {
        status,
        stdout,
        stderr,
    }
}

#[test]
fn cleared_removed_and_overridden_environment_has_no_parent_state_or_byte_loss() {
    const CASE: &str = "ASB_LAUNCH_ENV_CASE";
    if std::env::var_os(CASE).is_none() {
        let name = module_path!().split_once("::").unwrap().1.to_owned()
            + "::cleared_removed_and_overridden_environment_has_no_parent_state_or_byte_loss";
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .env(CASE, "child")
            .env("ASB_FAKE_PARENT_SECRET", "fixture-not-a-credential")
            .env("ASB_FAKE_OVERRIDE", "parent")
            .env("HOME", "fixture-parent-home")
            .args(["--exact", &name, "--nocapture"]);
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
    let mut launch = ProcessLaunch::new("/usr/bin/python3");
    launch.arg("-c").arg("import os,json; d={k:os.environb.get(k.encode(),b'').hex() for k in ['ASB_FAKE_PARENT_SECRET','ASB_FAKE_OVERRIDE','ASB_RAW','ASB_GATE_RECEIPT_FD','ASB_GATE_OWNER_PID','HOME']}; d['RAW_KEY']=os.environb.get(b'ASB_\\xff',b'').hex(); print(json.dumps(d))");
    let raw = OsString::from_vec(b"raw-\xff-value".to_vec());
    for clear in [false, true] {
        let output = execute(&launch, |command| {
            if clear {
                command.env("ASB_FAKE_OVERRIDE", "discard").env_clear();
            }
            command
                .env_remove("ASB_FAKE_PARENT_SECRET")
                .env("ASB_FAKE_OVERRIDE", "child")
                .env("ASB_RAW", &raw)
                .env(OsStr::from_bytes(b"ASB_\xff"), &raw);
        });
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ASB_FAKE_PARENT_SECRET"], "");
        assert_eq!(value["ASB_FAKE_OVERRIDE"], "6368696c64");
        assert_eq!(value["ASB_RAW"], "7261772dff2d76616c7565");
        assert_eq!(value["RAW_KEY"], "7261772dff2d76616c7565");
        assert_eq!(value["ASB_GATE_RECEIPT_FD"], "");
        assert_eq!(value["ASB_GATE_OWNER_PID"], "");
        if clear {
            assert_eq!(value["HOME"], "");
        } else {
            assert_eq!(value["HOME"], "666978747572652d706172656e742d686f6d65");
        }
    }
}

#[test]
fn child_only_path_cwd_stdio_and_appended_args_use_the_same_command() {
    let fixture = Fixture::new();
    let bin = fixture.0.join("bin");
    let cwd = fixture.0.join("workspace");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir(&cwd).unwrap();
    let client = bin.join("launch-client");
    crate::write_test_file(&fixture.0, &client, "#!/bin/sh\nprintf '%s;%s;%s;' \"$PWD\" \"$ASB_VALUE\" \"$1\"\n/bin/cat\nprintf diagnostic >&2\n").unwrap();
    std::fs::set_permissions(&client, std::fs::Permissions::from_mode(0o700)).unwrap();
    let input = fixture.0.join("input");
    let output_path = fixture.0.join("stdout");
    std::fs::write(&input, "owned input").unwrap();
    let output = execute(&ProcessLaunch::new("launch-client"), |command| {
        command
            .env_clear()
            .env("PATH", &bin)
            .env("ASB_VALUE", "configured")
            .current_dir(&cwd)
            .arg("appended argument")
            .stdin(std::fs::File::open(&input).unwrap())
            .stdout(std::fs::File::create(&output_path).unwrap());
    });
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"diagnostic");
    assert_eq!(
        std::fs::read_to_string(output_path).unwrap(),
        format!("{};configured;appended argument;owned input", cwd.display())
    );
}

#[test]
fn non_utf8_executable_arguments_and_explicit_client_arg0_are_preserved() {
    let fixture = Fixture::new();
    // Keep an applet's basename: /bin/echo may itself be a multicall image.
    // The complete executable pathname and argument must still preserve bytes.
    let directory = fixture.0.join(OsStr::from_bytes(b"client-\xff"));
    std::fs::create_dir(&directory).unwrap();
    let client = directory.join("echo");
    let image =
        std::env::var_os("ASB_TEST_MULTICALL_ECHO_IMAGE").unwrap_or_else(|| "/bin/echo".into());
    std::fs::copy(image, &client).unwrap();
    let mut launch = ProcessLaunch::new(&client);
    launch.arg(OsStr::from_bytes(b"argument-\xfe"));
    let (command, route) = launch.native(Some(&binary()));
    let target = route.target(&command);
    assert_eq!(
        target.program.as_os_str().as_bytes(),
        client.as_os_str().as_bytes()
    );
    assert_eq!(target.args, [OsString::from_vec(b"argument-\xfe".to_vec())]);
    let output = execute(&launch, |command| {
        command.env_clear();
    });
    assert!(output.status.success());
    assert_eq!(output.stdout, b"argument-\xfe\n");
    let mut launch = ProcessLaunch::new("/bin/sh");
    launch
        .arg0(OsStr::from_bytes(b"client-argv0-\xfd"))
        .arg("-c")
        .arg("printf '%s' \"$0\"");
    let output = execute(&launch, |command| {
        command.env_clear();
    });
    assert!(output.status.success());
    assert_eq!(output.stdout, b"client-argv0-\xfd");
}

#[test]
fn collector_view_is_the_original_target_not_gate_metadata_and_never_guesses_path() {
    let mut launch = ProcessLaunch::new("unknown-bare-client");
    launch.arg("prefix").arg0("custom-client-name");
    let (mut command, route) = launch.native(Some(Path::new("/current-asb")));
    command.env_clear().arg(OsStr::from_bytes(b"tail-\xff"));
    let target = route.target(&command);
    assert_eq!(command.get_program(), "/current-asb");
    assert_eq!(target.program, "unknown-bare-client");
    assert_eq!(
        target.args,
        [
            OsString::from("prefix"),
            OsString::from_vec(b"tail-\xff".to_vec())
        ]
    );
    assert!(target.path.is_none());
    command.env("PATH", "/child-only");
    assert_eq!(
        route.target(&command).path.as_deref(),
        Some(OsStr::new("/child-only"))
    );
}

#[test]
fn a_direct_route_cannot_be_validated_for_gate_admission() {
    let (mut command, route) = ProcessLaunch::new("/bin/true").native(None);
    let scope = crate::process_cleanup::test_support::Scope::new();
    let error = route.validate_gate(&mut command).err().unwrap().to_string();
    assert!(error.contains("requires a gated launch route"));
    assert_eq!(scope.count(), 0);
}

#[test]
fn invalid_command_inputs_fail_before_spawn_and_missing_client_is_not_success() {
    for invalid_env in [false, true] {
        let mut launch = ProcessLaunch::new("/bin/true");
        if !invalid_env {
            launch.arg(OsStr::from_bytes(b"invalid\0argument"));
        }
        let (mut command, route) = launch.native(Some(&binary()));
        if invalid_env {
            command.env("INVALID", OsStr::from_bytes(b"invalid\0value"));
        }
        let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
        assert_eq!(
            admission.spawn().err().unwrap().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    let fixture = Fixture::new();
    let (mut command, route) =
        ProcessLaunch::new("/bin/true").native(Some(&fixture.0.join("missing-runtime")));
    let admission = Admission::new(route.validate_gate(&mut command).unwrap()).unwrap();
    assert_eq!(
        admission.spawn().err().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    let output = execute(
        &ProcessLaunch::new("/missing/asb-launch-fixture-client"),
        |_| {},
    );
    assert_eq!(output.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&output.stderr).contains("No such file"));
}

#[test]
fn original_command_credentials_process_group_and_async_safe_callback_are_retained() {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    let (mut reader, writer) = UnixStream::pair().unwrap();
    reader
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let descriptor = writer.as_raw_fd();
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let mut launch = ProcessLaunch::new("/usr/bin/python3");
    launch.arg("-c").arg("import os; print(str(os.getuid())+':'+str(os.getgid())); assert os.getpgrp()==os.getppid()");
    let output = execute(&launch, |command| {
        command.env_clear().uid(uid).gid(gid).process_group(0);
        unsafe {
            command.pre_exec(move || {
                if libc::write(descriptor, b"configured".as_ptr().cast(), 10) != 10 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    });
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, format!("{uid}:{gid}\n").as_bytes());
    let mut receipt = [0; 10];
    reader.read_exact(&mut receipt).unwrap();
    assert_eq!(&receipt, b"configured");
    drop(writer);
    let mut extra = Vec::new();
    reader.read_to_end(&mut extra).unwrap();
    assert!(
        extra.is_empty(),
        "configuration callback must run exactly once"
    );
    // Keep Write intentionally exercised against an already closed peer; this
    // also proves the client did not inherit the CLOEXEC fixture descriptor.
    assert!(reader.write_all(b"after").is_err());
}
