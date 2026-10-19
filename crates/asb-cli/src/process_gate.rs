#![allow(unsafe_code)]

//! Internal process start gate used to close the collector attachment race.

use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

#[cfg(unix)]
pub(crate) fn install(
    command: &mut Command,
    gate_executable: &Path,
    target: &str,
    args: &[String],
) {
    use std::os::unix::process::CommandExt;
    let gate_executable = gate_executable.to_owned();
    let target = target.to_owned();
    let args = args.to_owned();
    let environment_changes = command
        .get_envs()
        .map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned)))
        .collect::<Vec<_>>();
    unsafe {
        command.pre_exec(move || {
            let mut values = vec![
                gate_executable.as_os_str().to_owned(),
                OsString::from("__asb-gate"),
                OsString::from(&target),
            ];
            values.extend(args.iter().map(OsString::from));
            let c_values = values
                .iter()
                .map(|value| {
                    std::ffi::CString::new(value.as_os_str().as_bytes()).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "gate argument contains NUL")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut pointers = c_values
                .iter()
                .map(|value| value.as_ptr())
                .collect::<Vec<_>>();
            pointers.push(std::ptr::null());
            let mut environment = std::env::vars_os().collect::<Vec<_>>();
            for (key, value) in &environment_changes {
                environment.retain(|(existing, _)| existing != key);
                if let Some(value) = value {
                    environment.push((key.clone(), value.clone()));
                }
            }
            let c_environment = environment
                .iter()
                .map(|(key, value)| {
                    std::ffi::CString::new(
                        [
                            key.as_os_str().as_bytes(),
                            b"=",
                            value.as_os_str().as_bytes(),
                        ]
                        .concat(),
                    )
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "gate environment contains NUL")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let mut environment_pointers = c_environment
                .iter()
                .map(|value| value.as_ptr())
                .collect::<Vec<_>>();
            environment_pointers.push(std::ptr::null());
            libc::execve(
                c_values[0].as_ptr(),
                pointers.as_ptr(),
                environment_pointers.as_ptr(),
            );
            Err(io::Error::last_os_error())
        });
    }
}

#[cfg(unix)]
pub(crate) fn resume(pid: u32) -> io::Result<()> {
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGCONT) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn pause_current() -> io::Result<()> {
    if unsafe { libc::raise(libc::SIGSTOP) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
pub(crate) fn run_gate(args: &[OsString]) -> std::process::ExitCode {
    let Some((program, target_args)) = args.split_first() else {
        return std::process::ExitCode::from(2);
    };
    if pause_current().is_err() {
        return std::process::ExitCode::from(2);
    }
    match Command::new(program).args(target_args).status() {
        Ok(status) => std::process::ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(_) => std::process::ExitCode::from(127),
    }
}
