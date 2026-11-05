//! Private fixed-size gate-authored admission/quiescence proof channel.
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;

pub(super) const ENV_FD: &str = "ASB_GATE_RECEIPT_FD";
const MAGIC: &[u8; 8] = b"ASBGATE2";
const FRAME: usize = 16;

pub(super) struct Sender(File);
impl Sender {
    pub(super) fn descriptor(&self) -> i32 {
        self.0.as_raw_fd()
    }
    pub(super) fn from_environment() -> io::Result<Option<Self>> {
        let value = std::env::var_os(ENV_FD);
        // This is the freshly exec'd, single-threaded gate, before admission.
        unsafe { std::env::remove_var(ENV_FD) };
        let Some(value) = value else {
            return Ok(None);
        };
        let descriptor = value
            .to_str()
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|fd| *fd >= 3)
            .ok_or_else(|| io::Error::other("invalid gate receipt descriptor"))?;
        // Never let an executed client inherit the gate's proof authority.
        if unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(descriptor) };
        if !file.metadata()?.file_type().is_socket() {
            return Err(io::Error::other(
                "gate receipt descriptor is not a private socket",
            ));
        }
        Ok(Some(Self(file)))
    }

    pub(super) fn hello(&mut self) -> io::Result<()> {
        self.send(1, 0)
    }
    pub(super) fn quiescent(&mut self, code: u8) -> io::Result<()> {
        self.send(2, code)
    }
    fn send(&mut self, kind: u8, code: u8) -> io::Result<()> {
        let mut frame = [0; FRAME];
        frame[..8].copy_from_slice(MAGIC);
        frame[8] = kind;
        frame[9] = code;
        frame[12..].copy_from_slice(&std::process::id().to_le_bytes());
        // Two tiny frames on an initially empty nonblocking socket. Failure is
        // explicit; never wait indefinitely for a departed/blocked caller.
        if write_retry(&mut self.0, &frame)? == FRAME {
            Ok(())
        } else {
            Err(io::Error::other("incomplete gate receipt write"))
        }
    }
}

pub(super) struct Receiver {
    socket: UnixStream,
    bytes: Vec<u8>,
    frames: usize,
    pub(super) hello: bool,
    pub(super) quiescent: Option<u8>,
}
impl Receiver {
    #[cfg(test)]
    pub(super) fn shut_read_for_test(&self) {
        self.socket.shutdown(std::net::Shutdown::Read).unwrap();
    }
    pub(super) fn pair() -> io::Result<(Self, UnixStream)> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        for socket in [&reader, &writer] {
            if unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok((
            Self {
                socket: reader,
                bytes: Vec::new(),
                frames: 0,
                hello: false,
                quiescent: None,
            },
            writer,
        ))
    }

    pub(super) fn poll(&mut self, expected_pid: u32) -> io::Result<()> {
        let mut buffer = [0; FRAME * 2 + 1];
        loop {
            match read_retry(&mut self.socket, &mut buffer) {
                Ok(0) => break, // EOF is never a quiescence proof.
                Ok(count) => {
                    if self.bytes.len() + count > FRAME * 2 {
                        return Err(io::Error::other("oversized gate receipt protocol"));
                    }
                    self.bytes.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        while self.bytes.len() >= (self.frames + 1) * FRAME {
            let frame = &self.bytes[self.frames * FRAME..(self.frames + 1) * FRAME];
            if &frame[..8] != MAGIC
                || frame[10..12] != [0, 0]
                || u32::from_le_bytes(frame[12..].try_into().expect("fixed frame")) != expected_pid
                || frame[8] != (self.frames + 1) as u8
                || self.frames == 0 && frame[9] != 0
            {
                return Err(io::Error::other("malformed or wrong-owner gate receipt"));
            }
            if self.frames == 0 {
                self.hello = true;
            } else {
                self.quiescent = Some(frame[9]);
            }
            self.frames += 1;
        }
        Ok(())
    }
}

fn read_retry(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn write_retry(writer: &mut impl Write, buffer: &[u8]) -> io::Result<usize> {
    loop {
        match writer.write(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_receipt_io_retries_without_inventing_a_receipt() {
        struct InterruptedOnce<T>(bool, T);
        impl<T: Read> Read for InterruptedOnce<T> {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if !self.0 {
                    self.0 = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.1.read(buffer)
            }
        }
        impl<T: Write> Write for InterruptedOnce<T> {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                if !self.0 {
                    self.0 = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.1.write(buffer)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.1.flush()
            }
        }
        let expected = frame(42, 2, 0);
        let mut writer = InterruptedOnce(false, Vec::new());
        assert_eq!(write_retry(&mut writer, &expected).unwrap(), FRAME);
        let mut reader = InterruptedOnce(false, writer.1.as_slice());
        let mut observed = [0; FRAME];
        assert_eq!(read_retry(&mut reader, &mut observed).unwrap(), FRAME);
        assert_eq!(observed, expected);
        assert_eq!(read_retry(&mut reader, &mut observed).unwrap(), 0);
    }

    fn frame(pid: u32, kind: u8, code: u8) -> [u8; FRAME] {
        let mut frame = [0; FRAME];
        frame[..8].copy_from_slice(MAGIC);
        frame[8] = kind;
        frame[9] = code;
        frame[12..].copy_from_slice(&pid.to_le_bytes());
        frame
    }

    #[test]
    fn fragmented_frames_validate_order_identity_and_code_without_eof() {
        let (mut receiver, mut writer) = Receiver::pair().unwrap();
        let duplicate = writer.try_clone().unwrap();
        let hello = frame(17, 1, 0);
        writer.write_all(&hello[..7]).unwrap();
        receiver.poll(17).unwrap();
        assert!(!receiver.hello);
        writer.write_all(&hello[7..]).unwrap();
        receiver.poll(17).unwrap();
        assert!(receiver.hello);
        assert_eq!(receiver.quiescent, None);
        writer.write_all(&frame(17, 2, 127)).unwrap();
        drop(writer);
        receiver.poll(17).unwrap();
        assert_eq!(receiver.quiescent, Some(127));
        drop(duplicate);
    }

    #[test]
    fn eof_missing_out_of_order_wrong_owner_and_oversize_are_not_proof() {
        let (mut receiver, writer) = Receiver::pair().unwrap();
        drop(writer);
        receiver.poll(9).unwrap();
        assert!(!receiver.hello);
        assert_eq!(receiver.quiescent, None);
        for payload in [
            frame(9, 2, 0).to_vec(),
            frame(8, 1, 0).to_vec(),
            frame(9, 1, 1).to_vec(),
            [frame(9, 1, 0), frame(9, 1, 0)].concat(),
            vec![0; FRAME * 2 + 1],
        ] {
            let (mut receiver, mut writer) = Receiver::pair().unwrap();
            writer.write_all(&payload).unwrap();
            assert!(receiver.poll(9).is_err());
            assert_eq!(receiver.quiescent, None);
        }
    }
}
