//! Optional private principal control. Ordinary gate receipts are unchanged.
#![allow(unsafe_code)]

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

pub(super) const ENV_FD: &str = "ASB_GATE_CONTROL_FD";
const MAGIC: &[u8; 8] = b"ASBCTRL1";
const FRAME: usize = 16;
const READY: u8 = 1;
const INTERRUPT: u8 = 2;
const ACK: u8 = 3;
const EXITED: u8 = 4;

#[cfg(test)]
std::thread_local! { static INTERRUPT_NEXT_READ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

fn invalid() -> io::Error {
    io::Error::other("invalid or out-of-order private gate control protocol")
}
fn frame(kind: u8, detail: u8, value: u32) -> [u8; FRAME] {
    let mut bytes = [0; FRAME];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8] = kind;
    bytes[9] = detail;
    bytes[12..].copy_from_slice(&value.to_le_bytes());
    bytes
}
fn decode(bytes: &[u8]) -> io::Result<(u8, u8, u32)> {
    if bytes.len() != FRAME || &bytes[..8] != MAGIC || bytes[10..12] != [0, 0] {
        return Err(invalid());
    }
    Ok((
        bytes[8],
        bytes[9],
        u32::from_le_bytes(bytes[12..].try_into().map_err(|_| invalid())?),
    ))
}
fn write_frame(socket: &mut UnixStream, bytes: &[u8; FRAME], deadline: Instant) -> io::Result<()> {
    let mut written = 0;
    while written < FRAME {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "private gate control write deadline exceeded",
            ));
        }
        match socket.write(&bytes[written..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "private gate control write stopped",
                ));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(
                Duration::from_millis(2).min(deadline.saturating_duration_since(Instant::now())),
            ),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Exact principal termination, not the guardian's synthetic exit code.
pub(crate) type PrincipalExit = crate::process_termination::ProcessTermination;

pub(super) struct Parent {
    socket: UnixStream,
    pending: Vec<u8>,
    frames: usize,
    pub ready: bool,
    requested: bool,
    pub acknowledged: bool,
    pub exited: Option<PrincipalExit>,
    failed: bool,
}
impl Parent {
    pub fn pair() -> io::Result<(Self, UnixStream)> {
        let (parent, child) = UnixStream::pair()?;
        for socket in [&parent, &child] {
            socket.set_nonblocking(true)?;
            if unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok((
            Self {
                socket: parent,
                pending: Vec::new(),
                frames: 0,
                ready: false,
                requested: false,
                acknowledged: false,
                exited: None,
                failed: false,
            },
            child,
        ))
    }
    pub fn interrupt(&mut self, deadline: Instant) -> io::Result<()> {
        if !self.ready || self.requested || self.exited.is_some() {
            return Err(invalid());
        }
        // Once any bytes might have been written this request is never retried.
        self.requested = true;
        write_frame(&mut self.socket, &frame(INTERRUPT, 0, 0), deadline)
    }
    pub fn poll(&mut self, gate: u32) -> io::Result<()> {
        let mut buffer = [0; FRAME * 3 + 1];
        let mut closed = false;
        loop {
            #[cfg(test)]
            let read = if INTERRUPT_NEXT_READ.replace(false) {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                self.socket.read(&mut buffer)
            };
            #[cfg(not(test))]
            let read = self.socket.read(&mut buffer);
            match read {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(count) => {
                    if self.pending.len() + count > FRAME * 3 {
                        return Err(invalid());
                    }
                    self.pending.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        if closed && !self.pending.len().is_multiple_of(FRAME) {
            return Err(invalid());
        }
        while self.pending.len() >= (self.frames + 1) * FRAME {
            let (kind, detail, value) =
                decode(&self.pending[self.frames * FRAME..(self.frames + 1) * FRAME])?;
            match kind {
                READY if !self.ready && self.frames == 0 && detail == 0 && value == gate => {
                    self.ready = true
                }
                ACK if self.ready
                    && self.requested
                    && !self.acknowledged
                    && self.exited.is_none()
                    && detail == 0
                    && value == 0 =>
                {
                    self.acknowledged = true
                }
                EXITED if self.ready && self.exited.is_none() => {
                    self.exited = Some(match (detail, value) {
                        (0, 0..=255) => PrincipalExit::Code(value as i32),
                        (1, 1..=127) => PrincipalExit::Signal(value as i32),
                        _ => return Err(invalid()),
                    });
                }
                _ => return Err(invalid()),
            }
            self.frames += 1;
        }
        Ok(())
    }

    pub fn observe(&mut self, gate: u32, finished: bool) {
        #[cfg(test)]
        if observation_fault::active() {
            self.failed = true;
            self.exited = None;
        }
        if !self.failed {
            self.failed = self.poll(gate).is_err()
                || finished && (self.pending.len() != self.frames * FRAME || self.exited.is_none());
            if self.failed {
                self.exited = None;
            }
        }
    }

    pub fn observation(&self) -> crate::process_termination::Observation {
        if self.failed {
            crate::process_termination::Observation::malformed()
        } else {
            crate::process_termination::Observation {
                process_termination: self.exited,
                process_termination_diagnostic: None,
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod observation_fault {
    static SHARED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    std::thread_local! { static ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    pub(crate) fn active() -> bool {
        ACTIVE.get() || SHARED.load(std::sync::atomic::Ordering::SeqCst)
    }
    /// Used only inside an isolated one-test subprocess owning all workers.
    pub(crate) struct SharedScope;
    impl SharedScope {
        pub(crate) fn enter() -> Self {
            assert!(!SHARED.swap(true, std::sync::atomic::Ordering::SeqCst));
            Self
        }
    }
    impl Drop for SharedScope {
        fn drop(&mut self) {
            SHARED.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
    pub(crate) struct Scope;
    impl Scope {
        pub(crate) fn enter() -> Self {
            assert!(!ACTIVE.replace(true));
            Self
        }
    }
    impl Drop for Scope {
        fn drop(&mut self) {
            ACTIVE.set(false);
        }
    }
}

pub(super) struct ChildControl {
    socket: UnixStream,
    request: Vec<u8>,
    requested: bool,
}
impl ChildControl {
    pub fn from_environment() -> io::Result<Option<Self>> {
        let value = std::env::var_os(ENV_FD);
        unsafe { std::env::remove_var(ENV_FD) };
        let Some(value) = value else {
            return Ok(None);
        };
        let fd = value
            .to_str()
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|fd| *fd >= 3)
            .ok_or_else(invalid)?;
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let socket = unsafe { UnixStream::from_raw_fd(fd) };
        let identity: std::os::fd::OwnedFd = socket.try_clone()?.into();
        let metadata = std::fs::File::from(identity).metadata()?;
        if !metadata.file_type().is_socket() {
            return Err(invalid());
        }
        socket.set_nonblocking(true)?;
        Ok(Some(Self {
            socket,
            request: Vec::new(),
            requested: false,
        }))
    }
    pub fn descriptor(&self) -> i32 {
        self.socket.as_raw_fd()
    }
    fn send(&mut self, bytes: [u8; FRAME]) -> io::Result<()> {
        write_frame(
            &mut self.socket,
            &bytes,
            Instant::now() + Duration::from_millis(100),
        )
    }
    pub fn ready(&mut self) -> io::Result<()> {
        self.send(frame(READY, 0, std::process::id()))
    }
    pub fn acknowledge(&mut self) -> io::Result<()> {
        self.send(frame(ACK, 0, 0))
    }
    pub fn exited(&mut self, status: ExitStatus) -> io::Result<()> {
        let (detail, value) = if let Some(code) = status.code() {
            (0, code as u32)
        } else if let Some(signal) = status.signal() {
            (1, signal as u32)
        } else {
            return Err(invalid());
        };
        self.send(frame(EXITED, detail, value))
    }
    pub fn take_interrupt(&mut self) -> io::Result<bool> {
        let mut buffer = [0; FRAME + 1];
        loop {
            match self.socket.read(&mut buffer) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "private gate control owner disconnected",
                    ));
                }
                Ok(count) => {
                    if self.requested || self.request.len() + count > FRAME {
                        return Err(invalid());
                    }
                    self.request.extend_from_slice(&buffer[..count]);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        if self.request.len() != FRAME {
            return Ok(false);
        }
        if decode(&self.request)? != (INTERRUPT, 0, 0) {
            return Err(invalid());
        }
        self.request.clear();
        self.requested = true;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_final_control_read_retries_before_classifying_exit() {
        let (mut parent, mut peer) = Parent::pair().unwrap();
        peer.write_all(&frame(READY, 0, 42)).unwrap();
        parent.poll(42).unwrap();
        peer.write_all(&frame(EXITED, 0, 7)).unwrap();
        INTERRUPT_NEXT_READ.set(true);
        parent.observe(42, true);
        assert_eq!(
            parent.observation().process_termination,
            Some(PrincipalExit::Code(7))
        );
    }
    #[test]
    fn finalization_rejects_partial_or_missing_exit_without_waiting_for_eof() {
        for payload in [
            vec![],
            frame(EXITED, 0, 7)[..7].to_vec(),
            [frame(EXITED, 0, 7).as_slice(), b"partial"].concat(),
        ] {
            let (mut parent, mut peer) = Parent::pair().unwrap();
            let held_writer = peer.try_clone().unwrap();
            peer.write_all(&frame(READY, 0, 42)).unwrap();
            parent.poll(42).unwrap();
            peer.write_all(&payload).unwrap();
            parent.observe(42, true);
            assert_eq!(
                parent.observation(),
                crate::process_termination::Observation::malformed()
            );
            drop(held_writer);
        }
    }
    #[test]
    fn capability_ack_and_exit_are_distinct_bounded_ordered_facts() {
        let (mut parent, mut peer) = Parent::pair().unwrap();
        let ready = frame(READY, 0, 42);
        peer.write_all(&ready[..3]).unwrap();
        parent.poll(42).unwrap();
        assert!(!parent.ready);
        peer.write_all(&ready[3..]).unwrap();
        parent.poll(42).unwrap();
        assert!(parent.ready);
        parent
            .interrupt(Instant::now() + Duration::from_secs(1))
            .unwrap();
        let mut command = [0; FRAME];
        peer.read_exact(&mut command).unwrap();
        assert_eq!(decode(&command).unwrap(), (INTERRUPT, 0, 0));
        peer.write_all(&frame(ACK, 0, 0)).unwrap();
        parent.poll(42).unwrap();
        assert!(parent.acknowledged);
        assert!(parent.exited.is_none());
        peer.write_all(&frame(EXITED, 1, libc::SIGINT as u32))
            .unwrap();
        parent.poll(42).unwrap();
        assert_eq!(parent.exited.unwrap().status().signal(), Some(libc::SIGINT));
        assert!(
            parent
                .interrupt(Instant::now() + Duration::from_secs(1))
                .is_err()
        );
    }
    #[test]
    fn malformed_duplicate_late_and_unowned_frames_never_become_completion() {
        for payload in [
            frame(READY, 0, 99).to_vec(),
            frame(ACK, 0, 0).to_vec(),
            [frame(READY, 0, 42), frame(READY, 0, 42)].concat(),
            [frame(READY, 0, 42), frame(EXITED, 2, 0)].concat(),
            vec![0; FRAME * 3 + 1],
        ] {
            let (mut parent, mut peer) = Parent::pair().unwrap();
            peer.write_all(&payload).unwrap();
            assert!(parent.poll(42).is_err());
            assert!(parent.exited.is_none());
        }
        let (mut parent, _peer) = Parent::pair().unwrap();
        assert!(parent.interrupt(Instant::now()).is_err());
    }
    #[test]
    fn child_accepts_one_fragmented_fixed_request_and_rejects_duplicate_or_owner_loss() {
        let (mut parent, peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut child = ChildControl {
            socket: peer,
            request: Vec::new(),
            requested: false,
        };
        let request = frame(INTERRUPT, 0, 0);
        parent.write_all(&request[..7]).unwrap();
        assert!(!child.take_interrupt().unwrap());
        parent.write_all(&request[7..]).unwrap();
        assert!(child.take_interrupt().unwrap());
        assert!(!child.take_interrupt().unwrap());
        parent.write_all(&request).unwrap();
        assert!(child.take_interrupt().is_err());
        drop(parent);
        // Parallel process fixtures can briefly inherit a writer between fork
        // and exec despite CLOEXEC. Require actual owner-channel EOF within a
        // fixed bound, not an immediate scheduler-dependent observation.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match child.take_interrupt() {
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
                    break;
                }
                Ok(false) => {
                    assert!(Instant::now() < deadline, "owner channel did not close");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Ok(true) => panic!("owner loss admitted another interruption"),
            }
        }
    }
    #[test]
    fn elapsed_write_budget_and_truncated_reply_are_not_success() {
        let (mut parent, mut peer) = Parent::pair().unwrap();
        peer.write_all(&frame(READY, 0, 42)).unwrap();
        parent.poll(42).unwrap();
        assert_eq!(
            parent.interrupt(Instant::now()).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(
            parent
                .interrupt(Instant::now() + Duration::from_secs(1))
                .is_err()
        );
        let (mut parent, mut peer) = Parent::pair().unwrap();
        peer.write_all(&frame(READY, 0, 42)[..8]).unwrap();
        drop(peer);
        // As in the neighboring owner-loss fixture, a parallel fork can retain
        // the CLOEXEC peer briefly. Require observed EOF, never immediate EOF.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let result = parent.poll(42);
            assert!(!parent.ready, "a truncated reply must never become READY");
            if result.is_err() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "truncated reply did not reach EOF"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!parent.ready);
    }
}
