//! Lifetime binding for the exclusive native gate, including its stopped phase.
#![allow(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);
static CANCELLATION_FD: AtomicI32 = AtomicI32::new(-1);
static CHILD_EVENT_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn cancel(_: libc::c_int) {
    CANCELLED.store(true, Ordering::Relaxed);
    let value = 1_u64;
    // Nonblocking eventfd write is async-signal-safe. Wake the separate child
    // even when cancellation races with the gate entering SIGSTOP.
    unsafe {
        libc::write(
            CANCELLATION_FD.load(Ordering::Relaxed),
            (&value as *const u64).cast(),
            std::mem::size_of_val(&value),
        );
    }
}

extern "C" fn child_event(_: libc::c_int) {
    let value = 1_u64;
    // A ptrace stop is reported as SIGCHLD.  The exclusive gate consumes the
    // corresponding wait status; this handler only makes its descriptor poll
    // wake without a timer or a syscall-sampling thread.
    unsafe {
        libc::write(
            CHILD_EVENT_FD.load(Ordering::Relaxed),
            (&value as *const u64).cast(),
            std::mem::size_of_val(&value),
        );
    }
}

pub(super) struct Owner {
    process: OwnedFd,
    cancellation: OwnedFd,
    child_events: OwnedFd,
    watcher_done: OwnedFd,
    watcher: libc::pid_t,
}

struct TraceState {
    root: libc::pid_t,
    tracees: BTreeMap<libc::pid_t, u64>,
    newborn: BTreeSet<libc::pid_t>,
    exited: BTreeSet<u64>,
    started: bool,
    root_status: Option<std::process::ExitStatus>,
}

impl TraceState {
    fn new(root: libc::pid_t) -> Self {
        Self {
            root,
            tracees: BTreeMap::from([(root, crate::agents::native_trace::ROOT_PROCESS)]),
            newborn: BTreeSet::new(),
            exited: BTreeSet::new(),
            started: false,
            root_status: None,
        }
    }
}

impl Owner {
    pub(super) fn bind(receipt_fd: Option<i32>, control_fd: Option<i32>) -> io::Result<Self> {
        let parent = unsafe { libc::getppid() };
        let expected = match std::env::var("ASB_GATE_OWNER_PID") {
            Ok(value) => value
                .parse::<libc::pid_t>()
                .map_err(|_| io::Error::other("invalid native gate owner identity"))?,
            Err(std::env::VarError::NotPresent) => parent,
            Err(_) => return Err(io::Error::other("invalid native gate owner identity")),
        };
        // The gate executable is still single threaded at this point.
        unsafe { std::env::remove_var("ASB_GATE_OWNER_PID") };
        if expected <= 1 || parent != expected {
            return Err(io::Error::other("native gate owner already exited"));
        }
        let process = pin(expected)?;
        let mut owner = Self {
            process,
            cancellation: event()?,
            child_events: event()?,
            watcher_done: event()?,
            watcher: -1,
        };
        if unsafe { libc::getppid() } != expected || owner.cancelled()? {
            return Err(io::Error::other("native gate owner exited during binding"));
        }
        // This handler belongs only to the exclusive gate executable, never
        // the shared runner or the client (exec resets caught dispositions).
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        action.sa_sigaction = cancel as *const () as usize;
        CANCELLATION_FD.store(owner.cancellation.as_raw_fd(), Ordering::Relaxed);
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        owner.install_child_waker()?;
        owner.watcher = owner.start_waker(receipt_fd, control_fd)?;
        Ok(owner)
    }

    pub(super) fn cancelled(&self) -> io::Result<bool> {
        if CANCELLED.load(Ordering::Relaxed) {
            return Ok(true);
        }
        let mut descriptor = libc::pollfd {
            fd: self.process.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(CANCELLED.load(Ordering::Relaxed))
            } else {
                Err(error)
            };
        }
        Ok(descriptor.revents != 0)
    }

    pub(super) fn wait(
        &self,
        child: &mut std::process::Child,
        mut control: Option<&mut super::control::ChildControl>,
    ) -> io::Result<std::process::ExitStatus> {
        let principal = pin(child.id() as libc::pid_t)?;
        let mut descriptors = [
            libc::pollfd {
                fd: self.process.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.cancellation.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: principal.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: control.as_ref().map_or(-1, |control| control.descriptor()),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            if self.cancelled()? {
                return Err(io::Error::other(
                    "native gate execution cancelled or owner exited",
                ));
            }
            if let Some(status) = child.try_wait()? {
                if let Some(control) = control.as_mut() {
                    // Optional observation cannot replace an observed wait
                    // status or independently valid base cleanup evidence.
                    let _ = control.exited(status);
                }
                return Ok(status);
            }
            let interrupt = match control.as_mut().map(|control| control.take_interrupt()) {
                Some(Ok(interrupt)) => interrupt,
                Some(Err(_)) => {
                    control = None;
                    descriptors[3].fd = -1;
                    false
                }
                None => false,
            };
            if interrupt {
                // Only the exact directly owned principal pinned before this
                // wait can receive this fixed operation. The guardian remains
                // uncancelled to permit graceful target flush and quiescence.
                let result = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        principal.as_raw_fd(),
                        libc::SIGINT,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                };
                if result == 0 {
                    if let Some(control) = control.as_mut() {
                        let _ = control.acknowledge();
                    }
                } else if io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                    return Err(io::Error::last_os_error());
                }
            }
            // No sampling thread or busy polling is added to the measured
            // interval. Principal exit, owner exit and cancellation all wake
            // this process through kernel descriptors.
            if unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    -1,
                )
            } < 0
            {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }

    /// Wait for a client launched under ptrace lifecycle observation.
    ///
    /// The gate stays the only reaper.  Unlike `PTRACE_SYSCALL`, the target is
    /// stopped only at process lifecycle transitions, so this has no
    /// per-syscall cost in the measured interval.  Kernel PIDs remain solely
    /// in this short-lived table; the durable trace contains trace-local IDs.
    pub(super) fn wait_traced(
        &self,
        child: &mut std::process::Child,
        mut control: Option<&mut super::control::ChildControl>,
        trace: &mut crate::agents::native_trace::GateWriter,
        ready: &OwnedFd,
        root_executable: Option<crate::agents::native_trace::Blob>,
    ) -> io::Result<std::process::ExitStatus> {
        let root = child.id() as libc::pid_t;
        attach_lifecycle_trace(root)?;
        let mut state = TraceState::new(root);
        let mut descriptors = [
            libc::pollfd {
                fd: self.process.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.cancellation.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.child_events.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: control.as_ref().map_or(-1, |control| control.descriptor()),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            if self.cancelled()? {
                let _ = self.abort_tracees(&mut state, trace, ready, &root_executable);
                return Err(io::Error::other(
                    "native gate execution cancelled or owner exited",
                ));
            }
            let next = match self.take_trace_events(&mut state, trace, ready, &root_executable) {
                Ok(next) => next,
                Err(error) => {
                    let _ = self.abort_tracees(&mut state, trace, ready, &root_executable);
                    return Err(error);
                }
            };
            if let Some(status) = next {
                if let Some(control) = control.as_mut() {
                    let _ = control.exited(status);
                }
                return Ok(status);
            }
            let interrupt = match control.as_mut().map(|control| control.take_interrupt()) {
                Some(Ok(interrupt)) => interrupt,
                Some(Err(_)) => {
                    control = None;
                    descriptors[3].fd = -1;
                    false
                }
                None => false,
            };
            if interrupt {
                self.interrupt_principal(root, control.as_deref_mut())?;
            }
            // `SIGCHLD` writes child_events for every ptrace transition. This
            // blocks until a real lifecycle, ownership, or control event—no
            // periodic timeout is used.
            if unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    -1,
                )
            } < 0
            {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            if descriptors[2].revents != 0 {
                drain_event(&self.child_events)?;
            }
        }
    }

    fn take_trace_events(
        &self,
        state: &mut TraceState,
        trace: &mut crate::agents::native_trace::GateWriter,
        ready: &OwnedFd,
        root_executable: &Option<crate::agents::native_trace::Blob>,
    ) -> io::Result<Option<std::process::ExitStatus>> {
        loop {
            if state.tracees.is_empty() {
                return Ok(state.root_status.take());
            }
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG | libc::__WALL) };
            if pid == 0 {
                return Ok(state
                    .tracees
                    .is_empty()
                    .then(|| state.root_status.take())
                    .flatten());
            }
            if pid < 0 {
                let error = io::Error::last_os_error();
                return if error.kind() == io::ErrorKind::Interrupted {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            if pid == self.watcher {
                // The wake-only child is never a tracee and must not acquire a
                // durable identity. Reaping it here is safe and bounded.
                continue;
            }
            let Some(&process) = state.tracees.get(&pid) else {
                // The gate is a subreaper: an orphaned descendant of a
                // detached helper can become its direct child. Its terminal
                // wait status has already settled that adopted child; it is
                // not a traced lifecycle event and has no trace identity.
                // Keep rejecting unknown stopped/live statuses, which could
                // indicate lost ptrace ownership.
                if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                    continue;
                }
                return Err(io::Error::other(
                    "native trace observed an unowned child status",
                ));
            };
            if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                if state.exited.insert(process) {
                    record(
                        trace,
                        process,
                        crate::agents::native_trace::Operation::ProcessExit,
                        None,
                    )?;
                }
                state.tracees.remove(&pid);
                if pid == state.root {
                    if !state.started {
                        return Err(io::Error::other(
                            "native trace principal exited before lifecycle observation started",
                        ));
                    }
                    state.root_status = Some(std::process::ExitStatus::from_raw(status));
                }
                continue;
            }
            if !libc::WIFSTOPPED(status) {
                continue;
            }
            let signal = libc::WSTOPSIG(status);
            let event = status >> 16;
            if !state.started && pid == state.root && signal == libc::SIGSTOP && event == 0 {
                set_lifecycle_options(pid)?;
                record_with_request(
                    trace,
                    process,
                    crate::agents::native_trace::Operation::ProcessStart,
                    None,
                    root_executable.clone(),
                )?;
                state.started = true;
                ptrace_continue(pid, 0)?;
                release_trace_launcher(ready)?;
                continue;
            }
            // The kernel reports the first stop of an inherited tracee with
            // architecture- and timing-dependent encodings (SIGSTOP,
            // PTRACE_EVENT_STOP, or a plain SIGTRAP). The lightweight
            // lifecycle backend intentionally records only the causal fork,
            // so detach at that first stop before interpreting an event or
            // forwarding any internal signal to the helper.
            if state.newborn.remove(&pid) {
                ptrace_detach(pid)?;
                state.tracees.remove(&pid);
                continue;
            }
            match event {
                libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE => {
                    let child = ptrace_event_pid(pid)?;
                    if state.tracees.contains_key(&child) {
                        return Err(io::Error::other(
                            "native trace reused a live child identity",
                        ));
                    }
                    let child_process = next_process_id(&state.tracees)?;
                    state.tracees.insert(child, child_process);
                    state.newborn.insert(child);
                    record(
                        trace,
                        child_process,
                        crate::agents::native_trace::Operation::ProcessFork,
                        Some(process),
                    )?;
                    ptrace_continue(pid, 0)?;
                }
                libc::PTRACE_EVENT_EXEC => {
                    record(
                        trace,
                        process,
                        crate::agents::native_trace::Operation::ProcessExec,
                        None,
                    )?;
                    ptrace_continue(pid, 0)?;
                }
                libc::PTRACE_EVENT_EXIT => {
                    if state.exited.insert(process) {
                        record(
                            trace,
                            process,
                            crate::agents::native_trace::Operation::ProcessExit,
                            None,
                        )?;
                    }
                    ptrace_continue(pid, 0)?;
                }
                _ => ptrace_continue(pid, signal)?,
            }
        }
    }

    fn interrupt_principal(
        &self,
        principal_pid: libc::pid_t,
        control: Option<&mut super::control::ChildControl>,
    ) -> io::Result<()> {
        let principal = pin(principal_pid)?;
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                principal.as_raw_fd(),
                libc::SIGINT,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result == 0 {
            if let Some(control) = control {
                let _ = control.acknowledge();
            }
            Ok(())
        } else if io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn abort_tracees(
        &self,
        state: &mut TraceState,
        trace: &mut crate::agents::native_trace::GateWriter,
        ready: &OwnedFd,
        root_executable: &Option<crate::agents::native_trace::Blob>,
    ) -> io::Result<()> {
        for pid in state.tracees.keys().copied() {
            // SIGKILL is not a replay policy decision: the gate itself is
            // already being cancelled. It makes an actively traced task enter
            // the same ptrace exit path which the sole gate reaps below.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        while !state.tracees.is_empty() {
            if self
                .take_trace_events(state, trace, ready, root_executable)?
                .is_some()
            {
                break;
            }
            let mut descriptor = libc::pollfd {
                fd: self.child_events.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut descriptor, 1, -1) } < 0
                && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
            {
                return Err(io::Error::last_os_error());
            }
            if descriptor.revents != 0 {
                drain_event(&self.child_events)?;
            }
        }
        Ok(())
    }

    fn install_child_waker(&self) -> io::Result<()> {
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        action.sa_sigaction = child_event as *const () as usize;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        CHILD_EVENT_FD.store(self.child_events.as_raw_fd(), Ordering::Relaxed);
        if unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) } < 0 {
            CHILD_EVENT_FD.store(-1, Ordering::Relaxed);
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn start_waker(
        &self,
        receipt_fd: Option<i32>,
        control_fd: Option<i32>,
    ) -> io::Result<libc::pid_t> {
        let gate = pin(std::process::id() as libc::pid_t)?;
        // A stopped process cannot run a watchdog thread. This tiny exclusive
        // child survives SIGSTOP of the gate, pins both process identities and
        // repeats SIGCONT after owner death, closing the check-before-stop
        // race. The gate remains the sole subreaper and cleanup authority.
        // Only async-signal-safe syscalls run after fork in the child.
        let child = unsafe { libc::fork() };
        if child < 0 {
            return Err(io::Error::last_os_error());
        }
        if child == 0 {
            unsafe {
                // fork does not honor CLOEXEC. Only the gate may author proofs;
                // its wake-only child must not retain the protocol socket.
                if let Some(fd) = receipt_fd {
                    libc::close(fd);
                }
                if let Some(fd) = control_fd {
                    libc::close(fd);
                }
                libc::close(self.child_events.as_raw_fd());
                for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                    // A terminal/owner-group signal must not kill the only
                    // process able to wake a stopped gate. Normal completion
                    // explicitly releases this watcher before tree cleanup.
                    libc::signal(signal, libc::SIG_IGN);
                }
                for fd in 0..=2 {
                    libc::close(fd);
                }
                let mut descriptors = [
                    libc::pollfd {
                        fd: self.process.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: gate.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: self.cancellation.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: self.watcher_done.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                loop {
                    let result = libc::poll(descriptors.as_mut_ptr(), 4, -1);
                    if result < 0 {
                        continue;
                    }
                    if descriptors[1].revents != 0 || descriptors[3].revents != 0 {
                        libc::_exit(0);
                    }
                    if descriptors[0].revents != 0 || descriptors[2].revents != 0 {
                        break;
                    }
                }
                let mut finished = [descriptors[1], descriptors[3]];
                loop {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        gate.as_raw_fd(),
                        libc::SIGCONT,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                    if libc::poll(finished.as_mut_ptr(), 2, 25) > 0 {
                        libc::_exit(0);
                    }
                }
            }
        }
        Ok(child)
    }

    pub(super) fn stop_waker(&self) {
        let value = 1_u64;
        unsafe {
            libc::write(
                self.watcher_done.as_raw_fd(),
                (&value as *const u64).cast(),
                8,
            );
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.stop_waker();
        CANCELLATION_FD.store(-1, Ordering::Relaxed);
        CHILD_EVENT_FD.store(-1, Ordering::Relaxed);
    }
}

fn record(
    trace: &mut crate::agents::native_trace::GateWriter,
    process: u64,
    operation: crate::agents::native_trace::Operation,
    parent_process: Option<u64>,
) -> io::Result<()> {
    trace
        .record_event(process, operation, parent_process)
        .map_err(|error| io::Error::other(error.to_string()))
}

fn record_with_request(
    trace: &mut crate::agents::native_trace::GateWriter,
    process: u64,
    operation: crate::agents::native_trace::Operation,
    parent_process: Option<u64>,
    request: Option<crate::agents::native_trace::Blob>,
) -> io::Result<()> {
    trace
        .record_event_with_request(process, operation, parent_process, request)
        .map_err(|error| io::Error::other(error.to_string()))
}

fn next_process_id(tracees: &BTreeMap<libc::pid_t, u64>) -> io::Result<u64> {
    tracees
        .values()
        .copied()
        .max()
        .unwrap_or(crate::agents::native_trace::ROOT_PROCESS)
        .checked_add(1)
        .ok_or_else(|| io::Error::other("native trace process identifier overflow"))
}

fn attach_lifecycle_trace(pid: libc::pid_t) -> io::Result<()> {
    let attached = unsafe {
        libc::ptrace(
            libc::PTRACE_ATTACH,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            std::ptr::null_mut::<libc::c_void>(),
        )
    };
    if attached != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_lifecycle_options(pid: libc::pid_t) -> io::Result<()> {
    let options = libc::PTRACE_O_TRACEFORK
        | libc::PTRACE_O_TRACEVFORK
        | libc::PTRACE_O_TRACECLONE
        | libc::PTRACE_O_TRACEEXEC
        | libc::PTRACE_O_TRACEEXIT;
    let result = unsafe {
        libc::ptrace(
            libc::PTRACE_SETOPTIONS,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            options as usize as *mut libc::c_void,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn release_trace_launcher(ready: &OwnedFd) -> io::Result<()> {
    let value = 1_u64;
    let result = unsafe {
        libc::write(
            ready.as_raw_fd(),
            (&value as *const u64).cast::<libc::c_void>(),
            std::mem::size_of_val(&value),
        )
    };
    if result == std::mem::size_of_val(&value) as isize {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn ptrace_event_pid(pid: libc::pid_t) -> io::Result<libc::pid_t> {
    let mut child: libc::c_ulong = 0;
    let result = unsafe {
        libc::ptrace(
            libc::PTRACE_GETEVENTMSG,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            (&mut child as *mut libc::c_ulong).cast::<libc::c_void>(),
        )
    };
    if result != 0 || child == 0 || child > libc::pid_t::MAX as libc::c_ulong {
        return Err(io::Error::last_os_error());
    }
    Ok(child as libc::pid_t)
}

fn ptrace_continue(pid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    let result = unsafe {
        libc::ptrace(
            libc::PTRACE_CONT,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            signal as usize as *mut libc::c_void,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn ptrace_detach(pid: libc::pid_t) -> io::Result<()> {
    let result = unsafe {
        libc::ptrace(
            libc::PTRACE_DETACH,
            pid,
            std::ptr::null_mut::<libc::c_void>(),
            std::ptr::null_mut::<libc::c_void>(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn drain_event(event: &OwnedFd) -> io::Result<()> {
    loop {
        let mut value = 0_u64;
        let result = unsafe {
            libc::read(
                event.as_raw_fd(),
                (&mut value as *mut u64).cast::<libc::c_void>(),
                std::mem::size_of_val(&value),
            )
        };
        if result == std::mem::size_of_val(&value) as isize {
            continue;
        }
        if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
            return Ok(());
        }
        return if result == 0 {
            Err(io::Error::other("native trace child event channel closed"))
        } else {
            Err(io::Error::last_os_error())
        };
    }
}

fn event() -> io::Result<OwnedFd> {
    let descriptor = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

fn pin(pid: libc::pid_t) -> io::Result<OwnedFd> {
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor as i32) })
    }
}
