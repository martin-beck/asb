//! One controller principal plus its actual host-transport custody boundary.
use super::super::ControllerCommand;
use std::io;
use std::process::{ChildStdin, ChildStdout, ExitStatus, Stdio};
use std::time::Instant;

#[cfg(target_os = "linux")]
pub(in crate::workloads::controller) struct Process(crate::process_gate::ManagedGate);

#[cfg(target_os = "linux")]
impl Process {
    pub(super) fn spawn(command: &mut ControllerCommand) -> Result<Self, String> {
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let route = command
            .route
            .take()
            .ok_or("controller command was already consumed")?;
        let validated = route
            .validate_gate(&mut command.command)
            .map_err(|error| error.to_string())?;
        let admission = crate::process_gate::Admission::with_workload_controller(validated)
            .map_err(|error| error.to_string())?;
        admission
            .spawn()
            .map(Self)
            .map_err(|error| format!("cannot start workload controller: {error}"))
    }
    pub(super) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.0.take_stdin()
    }
    pub(super) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.0.take_stdout()
    }
    pub(super) fn admit(&mut self) -> io::Result<()> {
        self.0.resume()
    }
    pub(in crate::workloads::controller) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.0.try_wait()?.is_none() {
            return Ok(None);
        }
        // A resource CLOSED event, the controller principal's status and actual
        // gate exit+QUIESCENT are independent. Only after the latter may this
        // method expose the original principal status (including a signal).
        self.0
            .principal_exit()
            .map(|exit| Some(exit.status()))
            .ok_or_else(|| io::Error::other("controller principal exit was not confirmed"))
    }
    pub(super) fn request_cancel(&mut self) -> io::Result<()> {
        self.0.request_cancel()
    }
    pub(super) fn retain(mut self, deadline: Instant) {
        // The existing proof-aware path never KILLs an admitted guardian. At
        // the caller's expired deadline it transfers its actual Child wait-only
        // to the keeper established before spawn, without a fresh Drop budget.
        let _ = self.0.stop_until(deadline);
    }
    #[cfg(test)]
    pub(in crate::workloads::controller) fn id(&self) -> u32 {
        self.0.id()
    }
    #[cfg(test)]
    pub(in crate::workloads::controller) fn lose_receipt(&self) {
        self.0.lose_receipt_for_test();
    }
}

#[cfg(not(target_os = "linux"))]
pub(in crate::workloads::controller) struct Process {
    child: std::process::Child,
    reaper: crate::process_cleanup::OwnedReaper,
}

#[cfg(not(target_os = "linux"))]
impl Process {
    pub(super) fn spawn(command: &mut ControllerCommand) -> Result<Self, String> {
        let reaper = crate::process_cleanup::OwnedReaper::new("workload-controller")?;
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| format!("cannot start workload controller: {error}"))?;
        Ok(Self { child, reaper })
    }
    pub(super) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }
    pub(super) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }
    pub(super) fn admit(&mut self) -> io::Result<()> {
        Ok(())
    }
    pub(in crate::workloads::controller) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
    #[allow(unsafe_code)]
    pub(super) fn request_cancel(&mut self) -> io::Result<()> {
        if unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    pub(super) fn retain(mut self, _deadline: Instant) {
        let _ = self.child.kill();
        self.reaper.adopt(self.child);
    }
    #[cfg(test)]
    pub(in crate::workloads::controller) fn id(&self) -> u32 {
        self.child.id()
    }
}
