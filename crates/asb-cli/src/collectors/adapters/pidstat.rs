//! Shared process-scoped sysstat preparation for the CPU and I/O adapters.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod parsing;
pub(super) use parsing::{Samples, parse_columns, parse_samples};

pub(super) fn executable(tools: Option<&Path>) -> PathBuf {
    if let Some(tools) = tools {
        for path in [
            tools.join("pidstat/bin/pidstat"),
            tools.join("mpstat/bin/pidstat"),
            tools.join("iostat/bin/pidstat"),
            tools.join("bin/pidstat"),
        ] {
            if path.is_file() {
                return path;
            }
        }
    }
    PathBuf::from("pidstat")
}

pub(super) fn preflight_unavailable(
    tools: Option<&Path>,
) -> Option<super::super::PreflightUnavailable> {
    let executable = executable(tools);
    super::super::PreflightUnavailable::missing_executable(&executable, tools, "pidstat")
}

pub(super) fn preflight(tools: Option<&Path>, flag: &str) -> Result<(), String> {
    let executable = executable(tools);
    let mut child = Command::new(&executable)
        .args([flag, "-T", "ALL", "-p", &std::process::id().to_string(), "1", "1"])
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().map_err(|error| {
            match error.kind() {
                std::io::ErrorKind::NotFound => "pidstat is required for the selected CPU or I/O collector. Install sysstat, then run `asb tool install mpstat` and retry.".to_owned(),
                std::io::ErrorKind::PermissionDenied => format!("cannot execute pidstat at {}: {error}. Check parent-directory access and, if you own this executable, run `chmod u+x -- {}`; then retry.", executable.display(), shell_quote(&executable)),
                _ => format!("cannot start pidstat at {}: {error}; check the installation and retry `asb tool install mpstat`", executable.display()),
            }
        })?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!(
                    "pidstat {flag} could not sample the current user process (exit {status}). Check procfs access and host security restrictions; inspect with `pidstat {flag} -p $$ 1 1`."
                ));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "pidstat {flag} preflight did not finish within 5 seconds; inspect it with `pidstat {flag} -p $$ 1 1`."
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("cannot check pidstat preflight: {error}"));
            }
        }
    }
}

/// Start the shared process-tree pidstat snapshot used by the CPU and I/O
/// collector adapters. The owning adapter supplies only its metric mode.
pub(super) fn start_snapshot(
    session: &mut crate::measurements::Session,
    name: &str,
    enabled: bool,
    pid: u32,
    mode: &str,
) -> Result<(), String> {
    let interval = crate::measurements::measurement_interval_ms()
        .div_ceil(1_000)
        .to_string();
    let runtime =
        Duration::from_secs(crate::measurements::measurement_interval_ms().div_ceil(1_000) + 5);
    let executable = executable(session.tool_dir.as_deref());
    let mode = mode.to_owned();
    session.start_tree_snapshot(
        name,
        enabled,
        &executable,
        pid,
        Box::new(move |pids| crate::measurements::SnapshotCommand {
            runtime,
            stream_bytes: 16 * 1024 * 1024,
            arguments: vec![
                mode.clone(),
                "-T".into(),
                "ALL".into(),
                "-p".into(),
                pids.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                interval.clone(),
                "1".into(),
            ],
        }),
    )
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}
