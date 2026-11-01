#![allow(unsafe_code)]

//! Internal process start gate used to close the collector attachment race.

use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::fs::File;
use std::io;
#[cfg(target_os = "linux")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::path::Path;
use std::process::Command;

#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};

#[cfg(target_os = "linux")]
const TRACE_READY_FD: &str = "ASB_NATIVE_TRACE_READY_FD";

#[cfg(target_os = "linux")]
const INSTALL_LOCK_FDS: &str = "ASB_GATE_INSTALL_LOCK_FDS";

/// Transfer installation ownership to the guardian until descendant cleanup.
#[cfg(target_os = "linux")]
pub(crate) fn retain_install_locks(command: &mut Command, fds: &[i32]) -> io::Result<()> {
    use std::os::unix::process::CommandExt;
    if fds.len() > 64 {
        return Err(io::Error::other(
            "too many installation ownership descriptors",
        ));
    }
    if fds.is_empty() {
        command.env_remove(INSTALL_LOCK_FDS);
        return Ok(());
    }
    command.env(
        INSTALL_LOCK_FDS,
        fds.iter().map(i32::to_string).collect::<Vec<_>>().join(","),
    );
    let fds = fds.to_vec();
    unsafe {
        command.pre_exec(move || {
            for fd in &fds {
                let flags = libc::fcntl(*fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn retain_guardian_install_locks() -> io::Result<()> {
    let value = std::env::var(INSTALL_LOCK_FDS).ok();
    unsafe { std::env::remove_var(INSTALL_LOCK_FDS) };
    let Some(value) = value else { return Ok(()) };
    let mut seen = std::collections::BTreeSet::new();
    for part in value.split(',') {
        let fd: i32 = part
            .parse()
            .map_err(|_| io::Error::other("invalid installer ownership descriptor"))?;
        if fd < 3 || !seen.insert(fd) || seen.len() > 64 {
            return Err(io::Error::other("invalid installer ownership descriptor"));
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A root executable identity is provenance for a later replay capsule, not
/// executable capture.  Reading no more than this amount keeps lifecycle
/// tracing lightweight and never adds a second copy of a large client binary
/// to the run directory.
#[cfg(target_os = "linux")]
const MAX_ROOT_EXECUTABLE_IDENTITY_BYTES: u64 = 8 * 1024 * 1024;

#[cfg(target_os = "linux")]
mod control;
#[cfg(all(test, target_os = "linux"))]
pub(crate) use control::observation_fault;
#[cfg(target_os = "linux")]
mod descendants;
#[cfg(target_os = "linux")]
mod managed;
#[cfg(target_os = "linux")]
mod owner;
#[cfg(target_os = "linux")]
mod receipt;
#[cfg(target_os = "linux")]
pub(crate) use descendants::DescendantScope;
#[cfg(target_os = "linux")]
pub(crate) use managed::{Admission, ManagedGate};

/// A loader-bundled runtime uses its launcher for recursive ASB execution.
pub(crate) fn executable() -> io::Result<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("ASB_RUNTIME_EXECUTABLE") {
        let path = std::path::PathBuf::from(path);
        if !path.is_absolute() || !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid ASB runtime launcher",
            ));
        }
        return Ok(path);
    }
    // The pre-exec child still runs this exact ASB image. Its kernel handle
    // remains valid if a simultaneous installation replaces the original
    // pathname; current_exe() can otherwise return an unusable '(deleted)' path.
    #[cfg(target_os = "linux")]
    return Ok(std::path::PathBuf::from("/proc/self/exe"));
    #[cfg(not(target_os = "linux"))]
    std::env::current_exe()
}

#[cfg(target_os = "linux")]
fn install_receipt(command: &mut Command, receipt_fd: i32) {
    use std::os::unix::process::CommandExt;
    command.env("ASB_GATE_OWNER_PID", std::process::id().to_string());
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(receipt_fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(receipt_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(unix)]
pub(crate) fn resume(child: &mut std::process::Child) -> io::Result<()> {
    wait_stopped(child)?;
    if unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGCONT) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
pub(crate) fn wait_stopped(child: &mut std::process::Child) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.try_wait()?.is_some() {
                return Err(io::Error::other(
                    "native process gate exited before readiness",
                ));
            }
            let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
            if status.lines().any(|line| {
                line.starts_with("State:") && line.split_whitespace().nth(1) == Some("T")
            }) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::other(
                    "native process gate readiness deadline exceeded",
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = child;
    Ok(())
}

#[cfg(unix)]
pub(crate) fn pause_current() -> io::Result<()> {
    if unsafe { libc::raise(libc::SIGSTOP) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Cooperatively cancel an owned gate, allowing its subreaper to finish.
/// Repeated CONT closes cancellation racing with the gate's initial STOP.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn cancel_gate(child: &mut std::process::Child) -> io::Result<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::other(
                "native gate cleanup deadline exceeded; descendant cleanup is unconfirmed; supervision must remain owned",
            ));
        }
        // This unreaped Child belongs exclusively to this caller; its PID
        // cannot be reused until the caller waits for it.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
            libc::kill(child.id() as libc::pid_t, libc::SIGCONT);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[cfg(unix)]
pub(crate) fn run_gate(args: &[OsString]) -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    if let Err(error) = retain_guardian_install_locks() {
        eprintln!("asb process gate cannot retain installer ownership: {error}");
        return std::process::ExitCode::from(2);
    }
    let (args, arg0) = if args
        .first()
        .is_some_and(|value| value == crate::launch::FORMAT)
    {
        match args.get(1).and_then(|value| value.to_str()) {
            Some("0") => (&args[2..], None),
            Some("1") if args.len() >= 4 => (&args[3..], Some(&args[2])),
            _ => return std::process::ExitCode::from(2),
        }
    } else {
        (args, None)
    };
    let Some((program, target_args)) = args.split_first() else {
        return std::process::ExitCode::from(2);
    };
    #[cfg(target_os = "linux")]
    let mut receipt = match receipt::Sender::from_environment() {
        Ok(receipt) => receipt,
        Err(error) => {
            eprintln!("asb process gate receipt setup failed: {error}");
            return std::process::ExitCode::from(2);
        }
    };
    #[cfg(target_os = "linux")]
    let mut descendants = match DescendantScope::new() {
        Ok(scope) => scope,
        Err(error) => {
            eprintln!("asb process gate could not retain orphaned descendants: {error}");
            return std::process::ExitCode::from(2);
        }
    };
    #[cfg(target_os = "linux")]
    let mut control = match control::ChildControl::from_environment() {
        Ok(control) => control,
        Err(error) => {
            eprintln!("asb process gate control setup failed: {error}");
            return std::process::ExitCode::from(2);
        }
    };
    #[cfg(target_os = "linux")]
    let owner = match owner::Owner::bind(
        receipt.as_ref().map(receipt::Sender::descriptor),
        control.as_ref().map(control::ChildControl::descriptor),
    ) {
        Ok(owner) => owner,
        Err(error) => {
            eprintln!("asb process gate could not bind its owner: {error}");
            return std::process::ExitCode::from(2);
        }
    };
    #[cfg(target_os = "linux")]
    let mut trace = match crate::agents::native_trace::GateWriter::from_environment() {
        Ok(trace) => trace,
        Err(error) => {
            eprintln!("asb process gate trace setup failed: {error}");
            return std::process::ExitCode::from(2);
        }
    };
    #[cfg(target_os = "linux")]
    if let Some(control) = control.as_mut()
        && let Err(error) = control.ready()
    {
        eprintln!("asb process gate control capability failed: {error}");
        return std::process::ExitCode::from(2);
    }
    #[cfg(target_os = "linux")]
    if let Some(receipt) = receipt.as_mut()
        && let Err(error) = receipt.hello()
    {
        eprintln!("asb process gate admission receipt failed: {error}");
        return std::process::ExitCode::from(2);
    }
    if pause_current().is_err() {
        return std::process::ExitCode::from(2);
    }
    #[cfg(target_os = "linux")]
    let status = (|| {
        if owner.cancelled()? {
            return Err(io::Error::other(
                "native gate owner exited before client execution",
            ));
        }
        // This is deliberately best-effort provenance.  A missing, changing,
        // non-regular, PATH-resolved, or oversized target must never block a
        // live agent.  The trace records only a digest and size, never the
        // target pathname or executable bytes.
        let root_executable = trace
            .as_ref()
            .and_then(|_| root_executable_identity(program));
        let mut command = target_command(program, target_args, arg0, trace.is_some())?;
        let trace_ready = trace
            .is_some()
            .then(|| prepare_trace_readiness(&mut command))
            .transpose()?;
        let mut child = command.spawn()?;
        let status = if let Some(trace) = trace.as_mut() {
            owner.wait_traced(
                &mut child,
                control.as_mut(),
                trace,
                trace_ready
                    .as_ref()
                    .expect("trace channel requires readiness"),
                root_executable,
            )?
        } else {
            owner.wait(&mut child, control.as_mut())?
        };
        Ok(status)
    })();
    #[cfg(not(target_os = "linux"))]
    let status =
        target_command(program, target_args, arg0, false).and_then(|mut command| command.status());
    #[cfg(target_os = "linux")]
    owner.stop_waker();
    #[cfg(target_os = "linux")]
    {
        let mut reported = false;
        while let Err(error) = if trace.is_some() {
            descendants.quiesce_traced()
        } else {
            descendants.quiesce()
        } {
            if !reported {
                eprintln!(
                    "asb process gate retains supervision: owned descendants are not yet quiescent: {error}"
                );
                reported = true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    let code = match status {
        Ok(status) => status.code().unwrap_or(1).clamp(0, 255) as u8,
        Err(error) => {
            eprintln!("asb process gate: {error}");
            127
        }
    };
    #[cfg(target_os = "linux")]
    if let Some(receipt) = receipt.as_mut()
        && let Err(error) = receipt.quiescent(code)
    {
        eprintln!("asb process gate completion receipt failed: {error}");
        return std::process::ExitCode::from(2);
    }
    std::process::ExitCode::from(code)
}

/// Return an opaque content identity for a direct regular-file launch target.
///
/// Bare names intentionally remain unavailable: resolving them through a
/// possibly mutable PATH would make the evidence claim stronger than what this
/// gate observed.  The execution itself retains normal PATH behavior.
#[cfg(target_os = "linux")]
fn root_executable_identity(
    program: &std::ffi::OsStr,
) -> Option<crate::agents::native_trace::Blob> {
    let path = Path::new(program);
    if !path.is_absolute() {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_ROOT_EXECUTABLE_IDENTITY_BYTES {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        bytes = bytes.checked_add(read as u64)?;
        if bytes > MAX_ROOT_EXECUTABLE_IDENTITY_BYTES {
            return None;
        }
        digest.update(&buffer[..read]);
    }
    // Do not claim an identity if the target changed while it was being read.
    // This is an observational hint only, but rejecting an unstable target
    // avoids attaching an incorrect digest to a later replay capsule.
    if bytes != metadata.len() || std::fs::metadata(path).ok()?.len() != metadata.len() {
        return None;
    }
    Some(crate::agents::native_trace::Blob {
        sha256: format!("{:x}", digest.finalize()),
        bytes,
    })
}

#[cfg(unix)]
fn target_command(
    program: &std::ffi::OsStr,
    args: &[OsString],
    arg0: Option<&OsString>,
    trace_lifecycle: bool,
) -> io::Result<Command> {
    #[cfg(target_os = "linux")]
    if trace_lifecycle {
        // The launcher stops before it can execute any client instruction.
        // That lets the gate seize this exact root with lifecycle options and
        // avoids a post-exec race or a per-syscall ptrace mode.
        let mut command = Command::new(executable()?);
        command
            .arg("__asb-native-trace-launch")
            .arg(crate::launch::FORMAT);
        match arg0 {
            Some(arg0) => {
                command.arg("1").arg(arg0);
            }
            None => {
                command.arg("0");
            }
        }
        command.arg(program).args(args);
        return Ok(command);
    }
    let mut launch = crate::launch::ProcessLaunch::new(program);
    for arg in args {
        launch.arg(arg);
    }
    if let Some(arg0) = arg0 {
        launch.arg0(arg0);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = trace_lifecycle;
    Ok(launch.direct())
}

/// Stop before replacing this small ASB launcher with the actual client.
///
/// This is an execution-order handshake, not a sandbox or policy boundary:
/// the client receives its original argv, environment and descriptors after
/// the gate has attached its lifecycle-only tracer.
#[cfg(unix)]
pub(crate) fn trace_launch(args: &[OsString]) -> std::process::ExitCode {
    use std::os::unix::process::CommandExt;

    let (args, arg0) = if args
        .first()
        .is_some_and(|value| value == crate::launch::FORMAT)
    {
        match args.get(1).and_then(|value| value.to_str()) {
            Some("0") => (&args[2..], None),
            Some("1") if args.len() >= 4 => (&args[3..], Some(&args[2])),
            _ => return std::process::ExitCode::from(2),
        }
    } else {
        return std::process::ExitCode::from(2);
    };
    let Some((program, target_args)) = args.split_first() else {
        return std::process::ExitCode::from(2);
    };
    #[cfg(target_os = "linux")]
    if !wait_for_trace_attachment() {
        return std::process::ExitCode::from(2);
    }
    let mut launch = crate::launch::ProcessLaunch::new(program);
    for arg in target_args {
        launch.arg(arg);
    }
    if let Some(arg0) = arg0 {
        launch.arg0(arg0);
    }
    let error = launch.direct().exec();
    eprintln!("asb native trace launcher could not execute client: {error}");
    std::process::ExitCode::from(127)
}

#[cfg(target_os = "linux")]
fn prepare_trace_readiness(command: &mut Command) -> io::Result<OwnedFd> {
    use std::os::unix::process::CommandExt;

    let descriptor = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    let ready = unsafe { OwnedFd::from_raw_fd(descriptor) };
    command.env(TRACE_READY_FD, descriptor.to_string());
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(descriptor, libc::F_GETFD);
            if flags < 0 || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(ready)
}

#[cfg(target_os = "linux")]
fn wait_for_trace_attachment() -> bool {
    let Some(value) = std::env::var_os(TRACE_READY_FD) else {
        return false;
    };
    unsafe { std::env::remove_var(TRACE_READY_FD) };
    let Some(descriptor) = value
        .to_str()
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|descriptor| *descriptor >= 3)
    else {
        return false;
    };
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
    {
        return false;
    }
    let mut value = 0_u64;
    loop {
        let result = unsafe {
            libc::read(
                descriptor,
                (&mut value as *mut u64).cast::<libc::c_void>(),
                std::mem::size_of_val(&value),
            )
        };
        if result == std::mem::size_of_val(&value) as isize {
            return value == 1;
        }
        if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return false;
    }
}

/// Keep descendants that daemonize or outlive an intermediate helper attached
/// to the measured root. Without a subreaper, Linux reparents those helpers
/// to PID 1 and process-tree collectors can no longer observe them. This is
/// set only in the short-lived gate child, never in the coordinator process.
#[cfg(target_os = "linux")]
fn become_child_subreaper() -> io::Result<()> {
    // SAFETY: prctl does not dereference pointers for these operations and is
    // scoped to the gate process.
    let result = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "process_gate/coverage_tests.rs"]
mod coverage_tests;
