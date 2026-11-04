//! Lifecycle of one adapter-owned sibling service in the guest supervisor.
use crate::Error;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) struct AuxiliaryService {
    child: Child,
    diagnostics: std::sync::mpsc::Receiver<Vec<u8>>,
}

/// Validated adapter service command, held until collectors are attached.
pub(crate) struct AuxiliaryPlan(Command);

impl AuxiliaryPlan {
    pub(crate) fn new(command: Command) -> Self {
        Self(command)
    }

    pub(crate) fn start(mut self) -> Result<AuxiliaryService, Error> {
        AuxiliaryService::start(&mut self.0)
    }
}

impl AuxiliaryService {
    pub(crate) fn start(command: &mut Command) -> Result<Self, Error> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| Error::Run("cannot start adapter auxiliary service".into()))?;
        let stdout = child.stdout.take().expect("piped service stdout");
        let stderr = child.stderr.take().expect("piped service stderr");
        let (diagnostics_send, diagnostics) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut stderr = stderr;
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        let retained = count.min(8192usize.saturating_sub(bytes.len()));
                        bytes.extend_from_slice(&buffer[..retained]);
                    }
                }
            }
            let _ = diagnostics_send.send(bytes);
        });
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = send.send(result);
        });
        match receive.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(line)) if line == "READY\n" => Ok(Self { child, diagnostics }),
            result => {
                let timed_out = matches!(result, Err(std::sync::mpsc::RecvTimeoutError::Timeout));
                let _ = child.kill();
                let _ = child.wait();
                let diagnostic = diagnostics
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap_or_default();
                let text = String::from_utf8_lossy(&diagnostic);
                let reason = if timed_out {
                    "readiness timed out"
                } else if text.contains("does not match the selected program and workspace effects")
                {
                    "recorded program or snapshot digest does not match"
                } else if text.contains("ENOENT") {
                    "required service input is missing"
                } else if text.contains("sidecar") {
                    "native sidecar validation failed"
                } else {
                    "service exited before readiness"
                };
                Err(Error::Run(format!(
                    "adapter auxiliary service did not become ready: {reason}"
                )))
            }
        }
    }
    pub(crate) fn child(&mut self) -> &mut Child {
        &mut self.child
    }
    /// Classify an early service exit without publishing its private output.
    pub(crate) fn early_exit(&mut self) -> Option<String> {
        let status = self.child.try_wait().ok().flatten()?;
        let diagnostic = self
            .diagnostics
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&diagnostic);
        let reason = if text.contains("EPIPE") {
            "broken pipe"
        } else if text.contains("ECONNRESET") {
            "connection reset"
        } else if text.contains("ERR_UNHANDLED_REJECTION")
            || text.contains("UnhandledPromiseRejection")
        {
            "unhandled promise rejection"
        } else if text.contains("out of memory") || text.contains("heap out of memory") {
            "out of memory"
        } else if text.contains("TypeError") {
            "type error"
        } else if text.contains("SyntaxError") {
            "syntax error"
        } else {
            "unclassified error"
        };
        #[cfg(unix)]
        let termination = {
            use std::os::unix::process::ExitStatusExt;
            status.code().map_or_else(
                || format!("signal {}", status.signal().unwrap_or_default()),
                |code| format!("exit {code}"),
            )
        };
        #[cfg(not(unix))]
        let termination = format!("exit {}", status.code().unwrap_or_default());
        Some(format!(
            "adapter auxiliary service exited early ({termination}; {reason})"
        ))
    }
    pub(crate) fn settle(&mut self) -> Result<(), Error> {
        self.child
            .stdin
            .take()
            .ok_or_else(|| Error::Run("adapter service settlement channel is unavailable".into()))?
            .write_all(b"S\n")
            .map_err(|_| Error::Run("adapter service settlement failed".into()))?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().map_err(crate::io_error)? {
                return if status.success() {
                    Ok(())
                } else {
                    let diagnostic = self
                        .diagnostics
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap_or_default();
                    let text = String::from_utf8_lossy(&diagnostic);
                    let reason = if text.contains("payload exceeds supported bounds") {
                        "native tool exchanges exceed the current recording size limit"
                    } else if text.contains("unfinished") {
                        "native tool calls were left unfinished"
                    } else if text.contains("exchange count is invalid") {
                        "native tool recording had no supported exchanges or too many exchanges"
                    } else if text.contains("not representable as JSON") {
                        "a native tool exchange could not be represented"
                    } else {
                        "inspect the retained guest and snapshot diagnostics"
                    };
                    Err(Error::Run(format!(
                        "adapter auxiliary service did not settle: {reason}"
                    )))
                };
            }
            if Instant::now() >= deadline {
                return Err(Error::Run(
                    "adapter auxiliary service settlement timed out".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for AuxiliaryService {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::AuxiliaryService;
    #[test]
    fn early_service_exit_reports_classification_without_private_output() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args([
            "-c",
            "printf 'READY\\n'; printf 'Error: EPIPE private-value\\n' >&2; exit 42",
        ]);
        let mut service = AuxiliaryService::start(&mut command).unwrap();
        service.child().wait().unwrap();
        let detail = service.early_exit().unwrap();
        assert!(detail.contains("exit 42"));
        assert!(detail.contains("broken pipe"));
        assert!(!detail.contains("private-value"));
    }

    #[test]
    fn live_service_is_not_misreported_as_early_exit() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "printf 'READY\\n'; read line"]);
        let mut service = AuxiliaryService::start(&mut command).unwrap();
        assert!(service.early_exit().is_none());
        service.settle().unwrap();
    }
    #[test]
    fn service_stderr_is_drained_after_the_diagnostic_limit() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args([
            "-c",
            "set -e; printf 'READY\\n'; head -c 65536 /dev/zero >&2; read line",
        ]);
        let mut service = AuxiliaryService::start(&mut command).unwrap();
        service.settle().unwrap();
    }
    #[test]
    fn service_failure_reports_only_the_classified_problem() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "printf 'READY\\n'; read line; printf 'Continue sidecar payload exceeds supported bounds private-value\\n' >&2; exit 1"]);
        let mut service = AuxiliaryService::start(&mut command).unwrap();
        let error = service.settle().unwrap_err().to_string();
        assert!(error.contains("size limit"));
        assert!(!error.contains("private-value"));
    }
    #[test]
    fn service_startup_classifies_mismatch_without_exposing_output() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "printf 'does not match the selected program and workspace effects private-value\\n' >&2; exit 1"]);
        let error = match AuxiliaryService::start(&mut command) {
            Ok(_) => panic!("service unexpectedly started"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("digest does not match"));
        assert!(!error.contains("private-value"));
    }
}
