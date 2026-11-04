//! Lifecycle of one adapter-owned sibling service in the guest supervisor.
use crate::Error;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) struct AuxiliaryService {
    child: Child,
}

impl AuxiliaryService {
    pub(crate) fn start(command: &mut Command) -> Result<Self, Error> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| Error::Run("cannot start adapter auxiliary service".into()))?;
        let stdout = child.stdout.take().expect("piped service stdout");
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = send.send(result);
        });
        match receive.recv_timeout(Duration::from_secs(15)) {
            Ok(Ok(line)) if line == "READY\n" => Ok(Self { child }),
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
                    Err(Error::Run(
                        "adapter auxiliary service did not settle".into(),
                    ))
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
