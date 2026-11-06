//! Legacy non-Unix transport. Its blocking pipes, reader and Drop are retained
//! for compatibility, NOT covered by the Unix bounded-I/O/custody guarantee.
use super::MAX_MESSAGE;
use crate::workloads::session::{ResourceDisposition, ResourceSettlement};
use crate::workloads::task_protocol::{
    CancellationAck, CompletedGrade, EventKind, NormalCloseAck, Request,
};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

pub(crate) struct Controller {
    child: Child,
    input: Option<ChildStdin>,
    events: Option<Receiver<Result<Value, String>>>,
}

impl Controller {
    // Legacy non-Unix Drop remains unqualified; do not imply that its blocking
    // I/O or unconditional wait can honor the Unix one-shot cleanup bound.
    pub(crate) fn limit_cleanup_to(&mut self, _deadline: Instant) {}
    #[cfg(test)]
    pub(super) fn child(&mut self) -> &mut Child {
        &mut self.child
    }
    pub(crate) fn spawn(command: &mut Command) -> Result<Self, String> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot start workload controller: {error}"))?;
        let input = child.stdin.take();
        let stdout = child.stdout.take().expect("piped controller stdout");
        let (sender, events) = mpsc::sync_channel(8);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                let result = reader
                    .by_ref()
                    .take(MAX_MESSAGE + 1)
                    .read_until(b'\n', &mut line);
                let event = match result {
                    Ok(0) => break,
                    Ok(_) if line.len() as u64 > MAX_MESSAGE || !line.ends_with(b"\n") => {
                        Err("workload controller message exceeded its framing limit".into())
                    }
                    Ok(_) => serde_json::from_slice(&line)
                        .map_err(|_| "workload controller returned invalid JSON".into()),
                    Err(_) => Err("cannot read workload controller response".into()),
                };
                let failed = event.is_err();
                if sender.send(event).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            input,
            events: Some(events),
        })
    }

    pub(crate) fn send(&mut self, request: &Value, deadline: Instant) -> Result<(), String> {
        if Instant::now() >= deadline {
            return Err("workload controller exceeded its request deadline".into());
        }
        let mut bytes =
            serde_json::to_vec(request).map_err(|_| "cannot encode workload request")?;
        if bytes.len() as u64 >= MAX_MESSAGE {
            return Err("workload request exceeds 1 MiB".into());
        }
        bytes.push(b'\n');
        self.input
            .as_mut()
            .ok_or("workload controller is closed")?
            .write_all(&bytes)
            .map_err(|_| "cannot send private workload request".to_owned())
    }

    pub(crate) fn receive(&mut self, deadline: Instant) -> Result<Value, String> {
        self.receive_until(deadline)?
            .ok_or_else(|| "workload controller exceeded its response deadline".to_owned())
    }

    pub(crate) fn receive_until(&mut self, deadline: Instant) -> Result<Option<Value>, String> {
        let event = match self.receive_raw(deadline) {
            Ok(event) => event,
            Err(error) if error == "workload controller exceeded its response deadline" => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if EventKind::of(&event) == Some(EventKind::Error) {
            return Err(super::controller_error(&event));
        }
        Ok(Some(event))
    }

    fn receive_raw(&mut self, deadline: Instant) -> Result<Value, String> {
        let event = self
            .events
            .as_ref()
            .ok_or("workload controller is closed")?
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => {
                    "workload controller exceeded its response deadline".to_owned()
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    "workload controller exited before completing its operation".to_owned()
                }
            })??;
        Ok(event)
    }

    /// EOF interrupts active work; continue draining its bounded event stream
    /// until the owner confirms teardown. Cancellation's nonzero child exit
    /// is expected, but it never substitutes for confirmed resource quiescence.
    pub(crate) fn cancel(&mut self, timeout: Duration) -> Result<ResourceDisposition, String> {
        let deadline = Instant::now() + timeout;
        self.input.take();
        loop {
            if Instant::now() >= deadline {
                return Err("workload cancellation exceeded its cleanup deadline".into());
            }
            let event = self.receive_raw(deadline)?;
            if EventKind::of(&event) == Some(EventKind::Closed) {
                let disposition = CancellationAck::decode(&event)?.disposition();
                self.wait_for_exit(deadline, false)?;
                return Ok(disposition);
            }
            // A command already in flight can produce output, an error or a
            // final result before the input reader observes EOF. No payload
            // from those discarded events is echoed to human diagnostics.
        }
    }

    pub(crate) fn request(
        &mut self,
        request: Value,
        expected: EventKind,
        timeout: Duration,
    ) -> Result<Value, String> {
        let deadline = Instant::now() + timeout;
        self.send(&request, deadline)?;
        let response = self.receive(deadline)?;
        if EventKind::of(&response) != Some(expected) {
            return Err("workload controller returned an unexpected protocol event".into());
        }
        Ok(response)
    }

    pub(in crate::workloads) fn finish(
        &mut self,
        _grade: CompletedGrade,
        timeout: Duration,
    ) -> Result<ResourceSettlement, String> {
        let deadline = Instant::now() + timeout;
        self.send(&Request::Close.value()?, deadline)?;
        // Keep stdin open until cleanup acknowledges completion. EOF is
        // cancellation and is only the fallback for a failed caller.
        let response = self.receive(deadline)?;
        let disposition = NormalCloseAck::decode(&response)?.disposition();
        self.input.take();
        self.wait_for_exit(deadline, false)?;
        let success = self
            .child
            .try_wait()
            .map_err(|_| "cannot check settled workload controller")?
            .ok_or("settled workload controller was not reaped")?
            .success();
        Ok(ResourceSettlement {
            disposition,
            diagnostic: if disposition != ResourceDisposition::Removed {
                Some("normal workload teardown did not remove resources; confirmed cancellation disposition retained".into())
            } else if !success {
                Some("workload controller failed after confirmed resource removal".into())
            } else {
                None
            },
        })
    }

    pub(crate) fn complete_response(&mut self, deadline: Instant) -> Result<(), String> {
        self.input.take();
        self.wait_for_exit(deadline, true)
    }

    fn wait_for_exit(&mut self, deadline: Instant, require_success: bool) -> Result<(), String> {
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|_| "cannot check completed workload controller")?
            {
                return if status.success() || !require_success {
                    Ok(())
                } else {
                    Err("workload controller failed after resource cleanup".into())
                };
            }
            if Instant::now() >= deadline {
                return Err("workload controller did not exit after resource cleanup".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        // EOF is cancellation, including while a command/evaluator is active.
        self.input.take();
        self.events.take();
        let started = Instant::now();
        loop {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            if started.elapsed() > Duration::from_secs(3) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        #[cfg(unix)]
        {
            // Signal only this owned child, never a process-name match/group.
            let _ = Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        while started.elapsed() < Duration::from_secs(15) {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
