//! Nonblocking owned Unix pipes. No detached reader, blocking stdin write,
//! thread join or unconditional child wait belongs to the caller's path.
use super::MAX_MESSAGE;
use crate::workloads::session::{ResourceDisposition, ResourceSettlement};
use crate::workloads::task_protocol::{
    CancellationAck, CompletedGrade, EventKind, NormalCloseAck, Request,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{ChildStdin, ChildStdout, ExitStatus};
use std::time::{Duration, Instant};

const EVENT_QUEUE: usize = 8;
const READ_QUANTUM: usize = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const DROP_EOF_GRACE: Duration = Duration::from_secs(3);
const DROP_CLEANUP_BUDGET: Duration = Duration::from_secs(15);

#[path = "process.rs"]
mod process;
use process::Process;

pub(crate) struct Controller {
    child: Option<Process>,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
    pending: Vec<u8>,
    scanned: usize,
    events: VecDeque<Result<Value, String>>,
    // Normal close, preserving cancellation and exit observation share the
    // earliest cleanup deadline. A failure must not restart a fresh budget.
    cleanup_deadline: Option<Instant>,
}

#[allow(unsafe_code)]
fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    #[cfg(all(test, target_os = "linux"))]
    if super::transport_tests::setup_failure() {
        return Err(io::Error::other("fixture nonblocking setup failure"));
    }
    let fd = pipe.as_raw_fd();
    // SAFETY: this borrowed pipe remains owned/open across both fcntl calls.
    // Preserve all original file-status flags except adding O_NONBLOCK.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn cleanup_poll(child: &mut Process) -> io::Result<Option<ExitStatus>> {
    #[cfg(all(test, target_os = "linux"))]
    if let Some(result) = super::transport_tests::cleanup_poll_override() {
        return result;
    }
    child.try_wait()
}

fn pause(deadline: Instant) {
    std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
}

impl Controller {
    pub(crate) fn spawn(command: &mut super::ControllerCommand) -> Result<Self, String> {
        let child = Process::spawn(command)?;
        // Establish the guard immediately; every fallible setup and unwind
        // after admission retains this same pre-established fallback custody.
        let mut owned = Self {
            child: Some(child),
            input: None,
            output: None,
            pending: Vec::new(),
            scanned: 0,
            events: VecDeque::new(),
            cleanup_deadline: None,
        };
        let child = owned.child.as_mut().expect("admitted controller");
        owned.input = child.take_stdin();
        owned.output = child.take_stdout();
        let setup = nonblocking(
            owned
                .input
                .as_ref()
                .ok_or("workload controller stdin is missing")?,
        )
        .and_then(|()| nonblocking(owned.output.as_ref().expect("piped controller stdout")))
        .map_err(|_| "cannot configure nonblocking workload controller pipes".to_owned());
        if let Err(error) = setup {
            let deadline = owned.cleanup_budget(Instant::now() + DROP_CLEANUP_BUDGET);
            return Err(match owned.stop_child(deadline) {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; {cleanup}"),
            });
        }
        if let Err(error) = owned.child.as_mut().expect("owned controller").admit() {
            return Err(format!("cannot admit workload controller: {error}"));
        }
        Ok(owned)
    }

    /// The caller chooses one absolute deadline before encoding or writing.
    /// While stdin is blocked, pump bounded stdout so duplex pressure cannot
    /// deadlock a child publishing an in-flight event before reading input.
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
        let result = self.write_request(&bytes, deadline);
        if result.is_err() {
            // A partial JSON line cannot be retried as another request. EOF
            // selects interruption/preservation, never destructive close.
            self.input.take();
        }
        result
    }

    fn write_request(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), String> {
        let mut sent = 0;
        while sent < bytes.len() {
            if Instant::now() >= deadline {
                return Err("workload controller exceeded its request deadline".into());
            }
            let progress = match self
                .input
                .as_mut()
                .ok_or("workload controller is closed")?
                .write(&bytes[sent..])
            {
                Ok(0) => return Err("cannot send private workload request".into()),
                Ok(count) => {
                    sent += count;
                    true
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
                Err(_) => return Err("cannot send private workload request".into()),
            };
            if sent == bytes.len() {
                return Ok(());
            }
            let read = self.read_available(deadline);
            if !progress && !read {
                pause(deadline);
            }
        }
        Ok(())
    }

    fn collect_events(&mut self) {
        while self.events.len() < EVENT_QUEUE {
            let Some(relative) = self.pending[self.scanned..]
                .iter()
                .position(|byte| *byte == b'\n')
            else {
                self.scanned = self.pending.len();
                break;
            };
            let end = self.scanned + relative;
            if end as u64 >= MAX_MESSAGE {
                self.read_failure("workload controller message exceeded its framing limit");
                return;
            }
            let line: Vec<_> = self.pending.drain(..=end).collect();
            self.scanned = 0;
            let event = serde_json::from_slice(&line)
                .map_err(|_| "workload controller returned invalid JSON".into());
            let failed = event.is_err();
            self.events.push_back(event);
            if failed {
                self.output.take();
                self.pending.clear();
                self.scanned = 0;
                return;
            }
        }
        if self.events.len() < EVENT_QUEUE && self.pending.len() as u64 > MAX_MESSAGE {
            self.read_failure("workload controller message exceeded its framing limit");
        }
    }

    fn read_failure(&mut self, error: &str) {
        self.output.take();
        self.pending.clear();
        self.scanned = 0;
        self.events.push_back(Err(error.into()));
    }

    fn read_available(&mut self, deadline: Instant) -> bool {
        let mut progress = false;
        let mut drained = 0;
        self.collect_events();
        while self.events.len() < EVENT_QUEUE && drained < READ_QUANTUM && Instant::now() < deadline
        {
            let Some(output) = self.output.as_mut() else {
                break;
            };
            let mut bytes = [0; 4096];
            let limit = bytes
                .len()
                .min(MAX_MESSAGE as usize + 1 - self.pending.len());
            match output.read(&mut bytes[..limit]) {
                Ok(0) => {
                    self.output.take();
                    if !self.pending.is_empty() {
                        self.read_failure("workload controller message exceeded its framing limit");
                    }
                    break;
                }
                Ok(count) => {
                    progress = true;
                    drained += count;
                    self.pending.extend_from_slice(&bytes[..count]);
                    self.collect_events();
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.read_failure("cannot read workload controller response");
                    break;
                }
            }
        }
        progress
    }

    pub(crate) fn receive(&mut self, deadline: Instant) -> Result<Value, String> {
        self.receive_until(deadline)?
            .ok_or_else(|| "workload controller exceeded its response deadline".to_owned())
    }

    /// Receive one event if it arrives before `deadline`. A deadline expiry is
    /// a normal polling result here; callers that require a hard deadline use
    /// `receive` instead.
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
        loop {
            if Instant::now() >= deadline {
                return Err("workload controller exceeded its response deadline".into());
            }
            if let Some(event) = self.events.pop_front() {
                return event;
            }
            self.read_available(deadline);
            if let Some(event) = self.events.pop_front() {
                return event;
            }
            if self.output.is_none() {
                return Err("workload controller exited before completing its operation".into());
            }
            pause(deadline);
        }
    }

    fn cleanup_budget(&mut self, proposed: Instant) -> Instant {
        let deadline = self
            .cleanup_deadline
            .map_or(proposed, |old| old.min(proposed));
        self.cleanup_deadline = Some(deadline);
        deadline
    }

    /// A one-shot helper's entire operation, including error cleanup, ends at
    /// this deadline. Long-lived task sessions instead retain their separate
    /// explicit preserving-cancellation budget after an execution/grade error.
    pub(crate) fn limit_cleanup_to(&mut self, deadline: Instant) {
        self.cleanup_budget(deadline);
    }

    pub(crate) fn cancel(&mut self, timeout: Duration) -> Result<ResourceDisposition, String> {
        let deadline = self.cleanup_budget(Instant::now() + timeout);
        self.input.take();
        loop {
            if Instant::now() >= deadline {
                return Err(self.handoff("workload cancellation exceeded its cleanup deadline"));
            }
            let event = match self.receive_raw(deadline) {
                Ok(event) => event,
                Err(error) => return Err(self.cleanup_failure(error, deadline)),
            };
            if EventKind::of(&event) == Some(EventKind::Closed) {
                let disposition = CancellationAck::decode(&event)?.disposition();
                self.wait_for_exit(deadline, false)?;
                return Ok(disposition);
            }
            // Deliberately discard arbitrary in-flight events until the owner
            // confirms removal/not-created/stopped preservation. Never echo them.
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
        let deadline = self.cleanup_budget(Instant::now() + timeout);
        let result = (|| {
            self.send(&Request::Close.value()?, deadline)?;
            let response = self.receive(deadline)?;
            let disposition = NormalCloseAck::decode(&response)?.disposition();
            self.input.take();
            self.wait_for_exit(deadline, false)?;
            let success = self
                .child
                .as_mut()
                .ok_or("workload controller cleanup already transferred")?
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
        })();
        result.map_err(|error| self.cleanup_failure(error, deadline))
    }

    pub(crate) fn complete_response(&mut self, deadline: Instant) -> Result<(), String> {
        let deadline = self.cleanup_budget(deadline);
        self.input.take();
        self.wait_for_exit(deadline, true)
    }

    fn wait_for_exit(&mut self, deadline: Instant, require_success: bool) -> Result<(), String> {
        loop {
            match cleanup_poll(
                self.child
                    .as_mut()
                    .ok_or("workload controller cleanup already transferred")?,
            ) {
                Ok(Some(status)) => {
                    return if status.success() || !require_success {
                        Ok(())
                    } else {
                        Err("workload controller failed after resource cleanup".into())
                    };
                }
                Err(_) => return Err(self.handoff("cannot check completed workload controller")),
                Ok(None) => {}
            }
            if Instant::now() >= deadline {
                return Err(self.handoff("workload controller did not exit after resource cleanup"));
            }
            pause(deadline);
        }
    }

    fn handoff(&mut self, reason: &str) -> String {
        self.close_io();
        if let Some(child) = self.child.take() {
            child.retain(Instant::now());
        }
        format!("{reason}; controller exit is unconfirmed, an owned cleanup reaper retains custody")
    }

    fn cleanup_failure(&mut self, error: String, deadline: Instant) -> String {
        if self.child.is_some() && Instant::now() >= deadline {
            self.handoff(&error)
        } else {
            error
        }
    }

    fn stop_child(&mut self, deadline: Instant) -> Result<(), String> {
        self.close_io();
        let grace = deadline.min(Instant::now() + DROP_EOF_GRACE);
        let mut signalled = false;
        loop {
            let Some(child) = self.child.as_mut() else {
                return Ok(());
            };
            match cleanup_poll(child) {
                Ok(Some(_)) => {
                    self.child.take();
                    return Ok(());
                }
                Err(_) => return Err(self.handoff("cannot confirm workload controller cleanup")),
                Ok(None) => {}
            }
            if Instant::now() >= deadline {
                return Err(self.handoff("workload controller cleanup exceeded its deadline"));
            }
            if !signalled && Instant::now() >= grace {
                let _ = child.request_cancel();
                signalled = true;
            }
            pause(deadline.min(if signalled { deadline } else { grace }));
        }
    }

    fn close_io(&mut self) {
        self.input.take();
        self.output.take();
        self.pending.clear();
        self.scanned = 0;
        self.events.clear();
    }

    #[cfg(test)]
    pub(super) fn child(&mut self) -> &mut Process {
        self.child.as_mut().expect("owned controller child")
    }
    #[cfg(test)]
    pub(super) fn stop_for_test(&mut self, timeout: Duration) -> Result<(), String> {
        self.stop_child(Instant::now() + timeout)
    }
    #[cfg(test)]
    pub(super) fn expire_cleanup_for_test(&mut self) {
        self.cleanup_deadline = Some(Instant::now());
    }
    #[cfg(test)]
    pub(super) fn buffered_for_test(&self) -> (usize, usize) {
        (self.events.len(), self.pending.len())
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn lose_receipt_for_test(&self) {
        self.child
            .as_ref()
            .expect("owned controller")
            .lose_receipt();
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        let deadline = self.cleanup_budget(Instant::now() + DROP_CLEANUP_BUDGET);
        let _ = self.stop_child(deadline);
    }
}
