//! Owned, proof-aware gate custody. A process exit alone is not tree quiescence.
use super::receipt::{self, Receiver};
use crate::process_cleanup::OwnedReaper;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus};
use std::time::{Duration, Instant};

const STARTUP: Duration = Duration::from_secs(5);
const CLEANUP: Duration = Duration::from_secs(6);

/// Create fallback custody and the private channel before admitting a child.
pub(crate) struct Admission<'a> {
    receiver: Receiver,
    writer: UnixStream,
    control: Option<(super::control::Parent, UnixStream)>,
    keeper: OwnedReaper,
    command: &'a mut Command,
    observation_only: bool,
    role: &'static str,
}

impl<'a> Admission<'a> {
    pub(crate) fn new(validated: crate::launch::ValidatedGate<'a>) -> io::Result<Self> {
        Self::prepare(validated, false, "command")
    }

    pub(crate) fn with_principal_control(
        validated: crate::launch::ValidatedGate<'a>,
    ) -> io::Result<Self> {
        Self::prepare(validated, true, "controlled-principal")
    }

    /// Request a second, actual-principal gate for agent-only collectors.
    pub(crate) fn with_measured_principal(
        validated: crate::launch::ValidatedGate<'a>,
    ) -> io::Result<Self> {
        let mut admission = Self::prepare(validated, true, "measured-principal")?;
        admission.command.env(super::MEASURE_PRINCIPAL, "1");
        admission
            .control
            .as_mut()
            .expect("controlled gate")
            .0
            .expect_principal();
        admission.observation_only = true;
        Ok(admission)
    }

    pub(crate) fn with_principal_observation(
        validated: crate::launch::ValidatedGate<'a>,
    ) -> io::Result<Self> {
        let mut admission = Self::prepare(validated, true, "observed-principal")?;
        admission.observation_only = true;
        Ok(admission)
    }

    /// A cancelled workload controller must finish stopping its owned guest.
    pub(crate) fn with_workload_controller(
        validated: crate::launch::ValidatedGate<'a>,
    ) -> io::Result<Self> {
        let mut admission = Self::with_principal_observation(validated)?;
        admission.role = "workload-controller";
        admission.command.env(super::WORKLOAD_CLEANUP, "1");
        Ok(admission)
    }

    fn prepare(
        validated: crate::launch::ValidatedGate<'a>,
        controlled: bool,
        role: &'static str,
    ) -> io::Result<Self> {
        let command = validated.into_command();
        command.env_remove(super::MEASURE_PRINCIPAL);
        command.env_remove(super::WORKLOAD_CLEANUP);
        let keeper = OwnedReaper::new("asb-gate-custody").map_err(io::Error::other)?;
        let (receiver, writer) = Receiver::pair()?;
        let fd = writer.as_raw_fd();
        command.env(receipt::ENV_FD, fd.to_string());
        super::install_receipt(command, fd);
        let control = if controlled {
            let (parent, peer) = super::control::Parent::pair()?;
            let fd = peer.as_raw_fd();
            command.env(super::control::ENV_FD, fd.to_string());
            super::install_receipt(command, fd);
            Some((parent, peer))
        } else {
            // Ordinary gates must never adopt a stale inherited control FD.
            command.env_remove(super::control::ENV_FD);
            None
        };
        Ok(Self {
            receiver,
            writer,
            control,
            keeper,
            command,
            observation_only: false,
            role,
        })
    }

    pub(crate) fn spawn(self) -> io::Result<ManagedGate> {
        let child = self.command.spawn()?;
        let Self {
            receiver,
            writer,
            keeper,
            control,
            observation_only,
            role,
            ..
        } = self;
        drop(writer);
        let control = control.map(|(parent, peer)| {
            drop(peer);
            parent
        });
        Ok(ManagedGate {
            child: Some(child),
            receiver,
            keeper,
            admitted: false,
            complete: None,
            observed: None,
            startup_deadline: Instant::now() + STARTUP,
            control,
            observation_only,
            role,
            #[cfg(test)]
            readiness_delay: None,
        })
    }
}

pub(crate) struct ManagedGate {
    child: Option<Child>,
    receiver: Receiver,
    keeper: OwnedReaper,
    admitted: bool,
    complete: Option<ExitStatus>,
    observed: Option<ExitStatus>,
    startup_deadline: Instant,
    control: Option<super::control::Parent>,
    observation_only: bool,
    role: &'static str,
    #[cfg(test)]
    readiness_delay: Option<Duration>,
}

impl ManagedGate {
    #[cfg(test)]
    pub(crate) fn lose_receipt_for_test(&self) {
        self.receiver.shut_read_for_test();
    }
    pub(crate) fn id(&self) -> u32 {
        self.child.as_ref().expect("owned gate").id()
    }

    pub(crate) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    pub(crate) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.as_mut()?.stdin.take()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    pub(crate) fn resume(&mut self) -> io::Result<()> {
        self.resume_when(|| true).map(|_| ())
    }

    /// Check an operation's cancellation/deadline after readiness, immediately
    /// before CONT. A false condition retains the stopped guardian in custody.
    pub(crate) fn resume_when(
        &mut self,
        continue_operation: impl FnOnce() -> bool,
    ) -> io::Result<bool> {
        self.wait_stopped()?;
        if !continue_operation() {
            return Ok(false);
        }
        if unsafe { libc::kill(self.id() as i32, libc::SIGCONT) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(true)
    }

    /// Resume the guardian, then wait for its exact direct principal to stop
    /// before exec. The caller may attach collectors and externally resume it.
    pub(crate) fn start_measured_principal(&mut self) -> io::Result<u32> {
        self.resume()?;
        let deadline = Instant::now() + STARTUP;
        loop {
            let gate = self.id();
            let control = self
                .control
                .as_mut()
                .ok_or_else(|| io::Error::other("measured principal control is missing"))?;
            control.poll(gate)?;
            if let Some(pid) = control.principal {
                let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
                let mut stopped = false;
                let mut parent = None;
                for line in status.lines() {
                    if line.starts_with("State:") {
                        stopped = line.split_whitespace().nth(1) == Some("T");
                    }
                    if let Some(value) = line.strip_prefix("PPid:") {
                        parent = value.trim().parse::<u32>().ok();
                    }
                }
                if stopped && parent == Some(gate) {
                    return Ok(pid);
                }
                if parent != Some(gate) {
                    return Err(io::Error::other("measured principal changed parent"));
                }
            }
            if self
                .child
                .as_mut()
                .expect("owned gate")
                .try_wait()?
                .is_some()
            {
                return Err(io::Error::other(
                    "guardian exited before principal readiness",
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "measured principal readiness deadline exceeded",
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Resume only this guardian's proven stopped principal, using a pidfd so
    /// an exit/PID-reuse race cannot signal an unrelated process.
    pub(crate) fn resume_measured_principal(&mut self, pid: u32) -> io::Result<()> {
        use std::os::fd::{FromRawFd, OwnedFd};
        let gate = self.id();
        let control = self
            .control
            .as_mut()
            .ok_or_else(|| io::Error::other("measured principal control is missing"))?;
        control.poll(gate)?;
        if control.principal != Some(pid) || control.exited.is_some() {
            return Err(io::Error::other(
                "measured principal differs from guardian custody",
            ));
        }
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful pidfd_open returns a newly owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
        let parent = status
            .lines()
            .find_map(|l| l.strip_prefix("PPid:"))
            .and_then(|p| p.trim().parse::<u32>().ok());
        let stopped = status
            .lines()
            .any(|l| l.starts_with("State:") && l.split_whitespace().nth(1) == Some("T"));
        if parent != Some(gate) || !stopped {
            return Err(io::Error::other(
                "measured principal is not stopped under its owned guardian",
            ));
        }
        if unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGCONT,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Establish current-ABI custody without admitting the application. This
    /// is also revalidated before CONT after any shared launch rendezvous.
    pub(crate) fn wait_stopped(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if let Some(delay) = self.readiness_delay.take() {
            std::thread::sleep(delay);
        }
        let deadline = self.startup_deadline;
        loop {
            self.receiver.poll(self.id()).map_err(incompatible)?;
            let id = self.id();
            if let Some(control) = self.control.as_mut() {
                control.poll(id).map_err(incompatible)?;
            }
            if let Some(status) = self.child.as_mut().expect("owned gate").try_wait()? {
                self.observed = Some(status);
                return Err(incompatible("gate exited before admission"));
            }
            if self.receiver.hello && self.control.as_ref().is_none_or(|control| control.ready) {
                let status = std::fs::read_to_string(format!("/proc/{}/status", self.id()))?;
                if status.lines().any(|line| {
                    line.starts_with("State:") && line.split_whitespace().nth(1) == Some("T")
                }) {
                    // From here this current-ABI gate owns all cleanup. Never
                    // send KILL to it, even if CONT or a later receipt fails.
                    self.admitted = true;
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(incompatible(
                    "no valid Hello and stopped readiness before deadline",
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.complete {
            return Ok(Some(status));
        }
        if self.child.is_none() {
            return Err(io::Error::other(
                "native gate retained in wait-only custody; cleanup is unconfirmed",
            ));
        }
        self.receiver.poll(self.id())?;
        let id = self.id();
        if let Some(control) = self.control.as_mut() {
            control.observe(id, false);
        }
        let status = match self.observed {
            Some(status) => Some(status),
            None => self.child.as_mut().expect("owned gate").try_wait()?,
        };
        self.observed = status;
        if let Some(status) = status {
            // Child exit synchronizes its final write, but a prior nonblocking
            // read may have raced it. Poll once more; never wait for EOF, which
            // may be held by a duplicated descriptor in a protocol fixture.
            self.receiver.poll(self.id())?;
            let id = self.id();
            if let Some(control) = self.control.as_mut() {
                control.observe(id, true);
            }
            let code = self.receiver.quiescent.ok_or_else(|| io::Error::other(format!(
                "native gate exited without a quiescence receipt; descendant cleanup is unconfirmed; role={} gate_pid={} exit={status:?}",
                self.role, self.id()
            )))?;
            if status.code() != Some(i32::from(code)) {
                return Err(io::Error::other(
                    "native gate exit disagrees with quiescence receipt",
                ));
            }
            self.complete = Some(status);
        }
        Ok(status)
    }

    #[cfg(test)]
    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn stop(&mut self) -> io::Result<ExitStatus> {
        self.stop_before(Instant::now() + CLEANUP)
    }

    /// A caller with an existing cleanup budget must not gain a fresh six
    /// seconds during transfer/Drop. Admitted guardians remain wait-only.
    pub(crate) fn stop_until(&mut self, deadline: Instant) -> io::Result<ExitStatus> {
        self.stop_before(deadline)
    }

    /// Request cancellation without surrendering proof-aware custody. Only an
    /// established current-ABI guardian may receive this fixed protocol signal.
    /// The caller must retain this gate until `try_wait` confirms quiescence.
    pub(crate) fn request_cancel(&mut self) -> io::Result<()> {
        if self.complete.is_some() || self.observed.is_some() {
            return Ok(());
        }
        if !self.admitted {
            self.wait_stopped()?;
        }
        for signal in [libc::SIGTERM, libc::SIGCONT] {
            if unsafe { libc::kill(self.id() as i32, signal) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// A request acknowledgement is not completion. Callers still need the
    /// existing guardian exit plus quiescence proof before parsing captures.
    pub(crate) fn interrupt_principal(&mut self, deadline: Instant) -> io::Result<()> {
        if self.observation_only
            || !self.admitted
            || self.complete.is_some()
            || self.observed.is_some()
        {
            return Err(io::Error::other(
                "principal interruption requires a live admitted controlled gate",
            ));
        }
        let id = self.id();
        let control = self
            .control
            .as_mut()
            .ok_or_else(|| io::Error::other("gate has no principal-control capability"))?;
        control.poll(id)?;
        control.interrupt(deadline)
    }

    pub(crate) fn principal_exit(&self) -> Option<super::control::PrincipalExit> {
        self.control.as_ref().and_then(|control| control.exited)
    }

    pub(crate) fn principal_observation(&self) -> crate::process_termination::Observation {
        self.control.as_ref().map_or_else(
            crate::process_termination::Observation::default,
            super::control::Parent::observation,
        )
    }

    pub(crate) fn interruption_acknowledged(&self) -> bool {
        self.control
            .as_ref()
            .is_some_and(|control| control.acknowledged)
    }

    fn stop_before(&mut self, deadline: Instant) -> io::Result<ExitStatus> {
        if let Some(status) = self.complete {
            return Ok(status);
        }
        if self.child.is_none() {
            return Err(io::Error::other(
                "native gate retained in wait-only custody; cleanup is unconfirmed",
            ));
        }
        if !self.admitted {
            return self.stop_starting_before(deadline, Child::try_wait);
        } else {
            loop {
                match self.try_wait() {
                    Ok(Some(status)) => return Ok(status),
                    Ok(None) => {}
                    Err(error) => {
                        if self.observed.is_none() {
                            unsafe {
                                libc::kill(self.id() as i32, libc::SIGTERM);
                                libc::kill(self.id() as i32, libc::SIGCONT);
                            }
                        }
                        self.handoff();
                        return Err(io::Error::other(format!(
                            "{error}; gate retained in wait-only custody"
                        )));
                    }
                }
                unsafe {
                    libc::kill(self.id() as i32, libc::SIGTERM);
                    libc::kill(self.id() as i32, libc::SIGCONT);
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        self.handoff();
        Err(io::Error::other(
            "native gate cleanup deadline exceeded; descendant cleanup is unconfirmed; gate retained in wait-only custody",
        ))
    }

    fn handoff(&mut self) {
        if let Some(child) = self.child.take() {
            self.keeper.adopt_wait_only(child);
        }
    }

    fn stop_starting_before(
        &mut self,
        deadline: Instant,
        mut poll: impl FnMut(&mut Child) -> io::Result<Option<ExitStatus>>,
    ) -> io::Result<ExitStatus> {
        // Never TERM or CONT an unknown/legacy stopped launcher. Its watcher
        // may translate TERM into CONT and thereby launch work.
        if let Some(status) = self.observed {
            self.complete = Some(status);
            return Ok(status);
        }
        let child = self.child.as_mut().expect("owned startup gate");
        let _ = child.kill();
        loop {
            match poll(child) {
                Ok(Some(status)) => {
                    self.complete = Some(status);
                    return Ok(status);
                }
                Ok(None) => {}
                Err(error) => {
                    self.handoff();
                    return Err(io::Error::other(format!(
                        "cannot confirm startup gate cleanup: {error}; gate retained in wait-only custody"
                    )));
                }
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.handoff();
        Err(io::Error::other(
            "startup gate cleanup deadline exceeded; gate retained in wait-only custody",
        ))
    }
}

impl Drop for ManagedGate {
    fn drop(&mut self) {
        let pid = self.child.as_ref().map(Child::id);
        if self.child.is_some()
            && self.complete.is_none()
            && let Err(error) = self.stop()
        {
            eprintln!(
                "asb process gate cleanup: role={} gate_pid={pid:?} admitted={} observed_exit={:?}: {error}",
                self.role, self.admitted, self.observed
            );
        }
    }
}

fn incompatible(reason: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!(
        "ASB process gate ABI is incompatible: {reason}; rebuild ASB and use the current ASB runtime"
    ))
}

#[cfg(test)]
mod tests;
