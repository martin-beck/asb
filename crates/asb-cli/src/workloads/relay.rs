//! Private, bounded TCP transport over an owned container exec's standard I/O.
//!
//! Destinations and credentials stay on the host. The guest exposes only
//! loopback listeners and opaque target/channel numbers; bytes are unmodified.
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OPEN: u8 = 1;
const DATA: u8 = 2;
const EOF: u8 = 3;
const CLOSE: u8 = 4;
const READY: u8 = 5;
const MAX_FRAME: usize = 64 * 1024;
const MAX_CHANNELS: usize = 128;
type Channels = Mutex<HashMap<u32, TcpStream>>;

struct State {
    output: Mutex<io::Stdout>,
    channels: Channels,
    stopping: AtomicBool,
    next: AtomicU32,
    readers: AtomicUsize,
}

struct ReaderGuard<'a>(&'a AtomicUsize);

impl Drop for ReaderGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid private relay frame")
}

fn read_frame(input: &mut impl Read) -> io::Result<Option<(u8, u32, Vec<u8>)>> {
    let mut header = [0; 9];
    // EOF between frames is normal; a partial header is not.
    if input.read(&mut header[..1])? == 0 {
        return Ok(None);
    }
    input.read_exact(&mut header[1..])?;
    let channel = u32::from_be_bytes(header[1..5].try_into().map_err(|_| invalid())?);
    let length = u32::from_be_bytes(header[5..9].try_into().map_err(|_| invalid())?) as usize;
    if length > MAX_FRAME {
        return Err(invalid());
    }
    let mut payload = vec![0; length];
    input.read_exact(&mut payload)?;
    Ok(Some((header[0], channel, payload)))
}

impl State {
    fn send(&self, kind: u8, channel: u32, data: &[u8]) -> io::Result<()> {
        let mut output = self.output.lock().map_err(|_| invalid())?;
        output.write_all(&[kind])?;
        output.write_all(&channel.to_be_bytes())?;
        output.write_all(&(data.len() as u32).to_be_bytes())?;
        output.write_all(data)?;
        output.flush()
    }

    fn close(&self, channel: u32) {
        if let Ok(mut channels) = self.channels.lock()
            && let Some(socket) = channels.remove(&channel)
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        if let Ok(mut channels) = self.channels.lock() {
            for (_, socket) in channels.drain() {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
    }
}

fn read_socket(state: &State, channel: u32, mut socket: TcpStream) {
    let mut buffer = [0; MAX_FRAME];
    while !state.stopping.load(Ordering::Acquire) {
        match socket.read(&mut buffer) {
            Ok(0) => {
                // Preserve the reverse direction until the host closes it.
                let _ = state.send(EOF, channel, &[]);
                return;
            }
            Ok(count) => {
                if state.send(DATA, channel, &buffer[..count]).is_err() {
                    state.stop();
                    return;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(_) => {
                state.close(channel);
                let _ = state.send(CLOSE, channel, &[]);
                return;
            }
        }
    }
}

fn accept(state: &Arc<State>, listener: &TcpListener, target: u32) -> io::Result<()> {
    let mut workers = Vec::new();
    while !state.stopping.load(Ordering::Acquire) {
        // Reap finished threads instead of retaining one handle per request.
        let mut index = 0;
        while index < workers.len() {
            let worker: &std::thread::JoinHandle<()> = &workers[index];
            if worker.is_finished() {
                let _ = workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_read_timeout(Some(Duration::from_millis(250)))?;
                socket.set_write_timeout(Some(Duration::from_secs(5)))?;
                let channel = state.next.fetch_add(1, Ordering::Relaxed);
                // Do not reuse a channel number within a transport lifetime.
                if channel == u32::MAX {
                    return Err(invalid());
                }
                {
                    let mut channels = state.channels.lock().map_err(|_| invalid())?;
                    if channels.len() >= MAX_CHANNELS
                        || workers.len() >= MAX_CHANNELS
                        || state.readers.load(Ordering::Acquire) >= MAX_CHANNELS
                    {
                        continue;
                    }
                    channels.insert(channel, socket.try_clone()?);
                    state.readers.fetch_add(1, Ordering::AcqRel);
                }
                state.send(OPEN, channel, &target.to_be_bytes())?;
                let state = Arc::clone(state);
                workers.push(std::thread::spawn(move || {
                    let _reader = ReaderGuard(&state.readers);
                    read_socket(&state, channel, socket)
                }));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

fn serve(addresses: &[std::net::IpAddr]) -> io::Result<()> {
    let listeners = addresses
        .iter()
        .map(|address| {
            let listener = TcpListener::bind((*address, 0))?;
            listener.set_nonblocking(true)?;
            Ok(listener)
        })
        .collect::<io::Result<Vec<_>>>()?;
    let ports = listeners
        .iter()
        .try_fold(Vec::new(), |mut ports, listener| {
            ports.extend_from_slice(&listener.local_addr()?.port().to_be_bytes());
            Ok::<_, io::Error>(ports)
        })?;
    let state = Arc::new(State {
        output: Mutex::new(io::stdout()),
        channels: Mutex::new(HashMap::new()),
        stopping: AtomicBool::new(false),
        next: AtomicU32::new(1),
        readers: AtomicUsize::new(0),
    });
    state.send(READY, 0, &ports)?;
    std::thread::scope(|scope| {
        for (target, listener) in listeners.iter().enumerate() {
            let state = &state;
            scope.spawn(move || {
                if accept(state, listener, target as u32).is_err() {
                    state.stop();
                }
            });
        }
        let result = (|| {
            let mut input = io::stdin().lock();
            while let Some((kind, channel, bytes)) = read_frame(&mut input)? {
                if channel == 0
                    || !matches!(kind, DATA | EOF | CLOSE)
                    || (kind != DATA && !bytes.is_empty())
                {
                    return Err(invalid());
                }
                if kind == CLOSE {
                    state.close(channel);
                    continue;
                }
                // Do not hold the registry mutex during potentially slow I/O.
                let socket = state
                    .channels
                    .lock()
                    .map_err(|_| invalid())?
                    .get(&channel)
                    .map(TcpStream::try_clone)
                    .transpose()?;
                if let Some(mut socket) = socket {
                    let result = if kind == DATA {
                        socket.write_all(&bytes)
                    } else {
                        socket.shutdown(Shutdown::Write)
                    };
                    if result.is_err() {
                        state.close(channel);
                        state.send(CLOSE, channel, &[])?;
                    }
                }
            }
            Ok(())
        })();
        state.stop();
        result
    })
}

pub(crate) fn entry(args: &[std::ffi::OsString]) -> std::process::ExitCode {
    let count = args
        .first()
        .and_then(|value| value.to_str())
        .and_then(|value| value.parse::<usize>().ok());
    let result = match count {
        Some(count) if (1..=crate::benchmark::MAX_CONCURRENCY).contains(&count) => {
            let addresses = if args.len() == 1 {
                Ok(vec![
                    std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
                    count
                ])
            } else if args.len() == count + 1 {
                args[1..]
                    .iter()
                    .map(|address| {
                        address
                            .to_str()
                            .and_then(|value| value.parse::<std::net::IpAddr>().ok())
                            .filter(std::net::IpAddr::is_loopback)
                            .ok_or_else(invalid)
                    })
                    .collect::<io::Result<Vec<_>>>()
            } else {
                Err(invalid())
            };
            addresses.and_then(|addresses| serve(&addresses))
        }
        _ => Err(invalid()),
    };
    if result.is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        eprintln!("private benchmark relay failed");
        std::process::ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_frames_reject_truncation_and_oversized_payloads() {
        assert!(read_frame(&mut &[][..]).unwrap().is_none());
        assert!(read_frame(&mut &[DATA, 0][..]).is_err());
        let mut frame = vec![DATA, 0, 0, 0, 1];
        frame.extend_from_slice(&((MAX_FRAME + 1) as u32).to_be_bytes());
        assert!(read_frame(&mut &frame[..]).is_err());
        frame[5..9].copy_from_slice(&1u32.to_be_bytes());
        assert!(read_frame(&mut &frame[..]).is_err());
        frame.push(42);
        assert_eq!(
            read_frame(&mut &frame[..]).unwrap(),
            Some((DATA, 1, vec![42]))
        );
    }
}
