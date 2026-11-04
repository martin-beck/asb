//! Lifecycle of one adapter-owned sibling service in the guest supervisor.
use crate::Error;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) struct AuxiliaryService {
    child: Child,
    diagnostics: std::sync::mpsc::Receiver<Vec<u8>>,
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
            let mut bytes = Vec::new();
            let _ = stderr.take(8192).read_to_end(&mut bytes);
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
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                Err(Error::Run(
                    "adapter auxiliary service did not become ready".into(),
                ))
            }
        }
    }
    pub(crate) fn child(&mut self) -> &mut Child {
        &mut self.child
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
    fn service_failure_reports_only_the_classified_problem() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "printf 'READY\\n'; read line; printf 'Continue sidecar payload exceeds supported bounds private-value\\n' >&2; exit 1"]);
        let mut service = AuxiliaryService::start(&mut command).unwrap();
        let error = service.settle().unwrap_err().to_string();
        assert!(error.contains("size limit"));
        assert!(!error.contains("private-value"));
    }
}
