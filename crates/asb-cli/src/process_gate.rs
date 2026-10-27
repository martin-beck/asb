#![allow(unsafe_code)]

//! Internal process start gate used to close the collector attachment race.

use std::ffi::OsString;
use std::io;
use std::process::Command;

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
        let mut child = target_command(program, target_args, arg0).spawn()?;
        if let Some(trace) = trace.as_mut() {
            trace
                .record(crate::agents::native_trace::Operation::ProcessStart)
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        let status = owner.wait(&mut child, control.as_mut())?;
        if let Some(trace) = trace.as_mut() {
            trace
                .record(crate::agents::native_trace::Operation::ProcessExit)
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        Ok(status)
    })();
    #[cfg(not(target_os = "linux"))]
    let status = target_command(program, target_args, arg0).status();
    #[cfg(target_os = "linux")]
    owner.stop_waker();
    #[cfg(target_os = "linux")]
    {
        let mut reported = false;
        while let Err(error) = descendants.quiesce() {
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

#[cfg(unix)]
fn target_command(
    program: &std::ffi::OsStr,
    args: &[OsString],
    arg0: Option<&OsString>,
) -> Command {
    let mut launch = crate::launch::ProcessLaunch::new(program);
    for arg in args {
        launch.arg(arg);
    }
    if let Some(arg0) = arg0 {
        launch.arg0(arg0);
    }
    launch.direct()
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
