//! Content-free observation grammar shared by proxy and tool producers.
//!
//! The transport is deliberately separate from tracefs.  Child processes can
//! submit only this fixed grammar; the collector retains the pinned marker and
//! decides whether an observation is accepted and written.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EventKind {
    Begin,
    End,
    Instant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Event {
    pub kind: EventKind,
    pub subject: &'static str,
    pub cookie: u64,
    pub reason: Option<&'static str>,
}

const MAX_LINE: usize = 128;

/// A one-way, private descriptor passed to exactly one ASB-owned producer.
/// The relay retains the tracefs writer and tolerates observation failure.
pub(super) struct Relay {
    writer: File,
    state: Arc<State>,
    worker: Option<thread::JoinHandle<()>>,
}

struct State {
    marker: Mutex<Option<File>>,
    pid: AtomicU64,
    trace_id: Mutex<Option<String>>,
    stopping: AtomicBool,
    accepted: AtomicU64,
    rejected: AtomicU64,
    write_errors: AtomicU64,
}

impl Relay {
    pub(super) fn new() -> Result<Self, String> {
        let (reader, writer) = UnixStream::pair().map_err(|error| error.to_string())?;
        reader
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(|error| error.to_string())?;
        let writer = file_from_stream(writer)?;
        let state = Arc::new(State {
            marker: Mutex::new(None),
            pid: AtomicU64::new(0),
            trace_id: Mutex::new(None),
            stopping: AtomicBool::new(false),
            accepted: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            write_errors: AtomicU64::new(0),
        });
        let worker_state = Arc::clone(&state);
        let worker = thread::Builder::new()
            .name("asb-perfetto-observer".into())
            .spawn(move || run(reader, worker_state))
            .map_err(|error| error.to_string())?;
        Ok(Self {
            writer,
            state,
            worker: Some(worker),
        })
    }

    pub(super) fn activate(&self, marker: &File, pid: u32, trace_id: &str) -> Result<(), String> {
        let copy = marker.try_clone().map_err(|error| error.to_string())?;
        *self
            .state
            .marker
            .lock()
            .map_err(|_| "Perfetto observation lock poisoned")? = Some(copy);
        *self
            .state
            .trace_id
            .lock()
            .map_err(|_| "Perfetto observation lock poisoned")? = Some(trace_id.to_owned());
        self.state.pid.store(u64::from(pid), Ordering::Release);
        Ok(())
    }

    /// Duplicate the producer endpoint and clear close-on-exec only for that
    /// duplicate. The tracefs marker itself never crosses this boundary.
    pub(super) fn producer_fd(&self) -> Result<File, String> {
        let file = self.writer.try_clone().map_err(|error| error.to_string())?;
        set_inheritable(&file)?;
        Ok(file)
    }

    pub(super) fn finish(&mut self) {
        self.state.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.finish();
    }
}

fn run(reader: UnixStream, state: Arc<State>) {
    let mut reader = BufReader::new(reader);
    loop {
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => match parse(&line) {
                Ok(event) => deliver(&state, &event),
                Err(_) => {
                    state.rejected.fetch_add(1, Ordering::Relaxed);
                }
            },
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                if state.stopping.load(Ordering::Acquire) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn deliver(state: &State, event: &Event) {
    let pid = state.pid.load(Ordering::Acquire) as u32;
    let trace_id = state.trace_id.lock().ok().and_then(|value| value.clone());
    let Some(trace_id) = trace_id.filter(|_| pid > 1) else {
        state.rejected.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Ok(mut marker_file) = state.marker.lock() else {
        state.write_errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Some(marker_file) = marker_file.as_mut() else {
        state.rejected.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if writeln!(marker_file, "{}", marker(event, pid, &trace_id)).is_ok() {
        state.accepted.fetch_add(1, Ordering::Relaxed);
    } else {
        state.write_errors.fetch_add(1, Ordering::Relaxed);
    }
}

fn file_from_stream(stream: UnixStream) -> Result<File, String> {
    let fd = stream.as_raw_fd();
    std::mem::forget(stream);
    // Ownership moves exactly once from the UnixStream to File.
    #[allow(unsafe_code)]
    unsafe {
        Ok(File::from_raw_fd(fd))
    }
}

fn set_inheritable(file: &File) -> Result<(), String> {
    #[allow(unsafe_code)]
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFD);
        if flags < 0 || libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(())
}

/// Parse one strict, content-free producer message.
pub(super) fn parse(input: &[u8]) -> Result<Event, &'static str> {
    if input.is_empty() || input.len() > MAX_LINE || !input.is_ascii() {
        return Err("malformed");
    }
    let text = std::str::from_utf8(input)
        .map_err(|_| "malformed")?
        .trim_end_matches('\n');
    let parts = text.split(' ').collect::<Vec<_>>();
    let kind = match parts.first().copied() {
        Some("begin") => EventKind::Begin,
        Some("end") => EventKind::End,
        Some("instant") => EventKind::Instant,
        _ => return Err("malformed"),
    };
    let subject = match parts.get(1).copied() {
        Some("llm_request") => "llm_request",
        Some("tool") => "tool",
        _ => return Err("subject"),
    };
    let cookie = parts
        .get(2)
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .ok_or("cookie")?;
    let reason = match (kind, parts.get(3).copied(), parts.len()) {
        (EventKind::Instant, Some("http_error"), 4) => Some("http_error"),
        (EventKind::Instant, Some("cancelled"), 4) => Some("cancelled"),
        (EventKind::Instant, Some("error"), 4) => Some("error"),
        (EventKind::Instant, _, _) => return Err("reason"),
        (_, None, 3) => None,
        _ => return Err("arity"),
    };
    Ok(Event {
        kind,
        subject,
        cookie,
        reason,
    })
}

/// Render one marker after collector-side validation. The name contains only
/// ASB-controlled labels, a trace identity and an opaque numeric cookie.
pub(super) fn marker(event: &Event, pid: u32, trace_id: &str) -> String {
    let subject = match event.subject {
        "llm_request" => "asb.llm.request",
        "tool" => "asb.tool",
        _ => unreachable!("parser owns subjects"),
    };
    let name = match event.reason {
        Some(reason) => format!("{subject}.{reason}.{trace_id}"),
        None => format!("{subject}.{trace_id}"),
    };
    match event.kind {
        EventKind::Begin => format!("S|{pid}|{name}|{}", event.cookie),
        EventKind::End => format!("F|{pid}|{name}|{}", event.cookie),
        EventKind::Instant => format!("I|{pid}|{name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_fixed_content_free_observations() {
        let event = parse(b"begin llm_request 7\n").unwrap();
        assert_eq!(event.kind, EventKind::Begin);
        assert_eq!(marker(&event, 42, "42-0"), "S|42|asb.llm.request.42-0|7");
        let failure = parse(b"instant tool 3 http_error\n").unwrap();
        assert_eq!(
            marker(&failure, 42, "42-0"),
            "I|42|asb.tool.http_error.42-0"
        );
    }

    #[test]
    fn rejects_content_and_unbounded_or_malformed_input() {
        for input in [
            b"begin llm_request 0\n".as_slice(),
            b"begin llm_request 1 https://provider.example\n",
            b"instant llm_request 1 prompt\n",
            b"begin tool 1\0\n",
        ] {
            assert!(parse(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn relay_keeps_marker_private_and_writes_only_valid_events_after_activation() {
        let root = std::env::temp_dir().join(format!("asb-observer-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let marker = root.join("trace_marker");
        let marker_file = File::create(&marker).unwrap();
        let mut relay = Relay::new().unwrap();
        let mut producer = relay.producer_fd().unwrap();
        writeln!(producer, "begin llm_request 4").unwrap();
        // Pre-activation input is accounted as unavailable rather than opening
        // or guessing a tracefs writer in the child.
        thread::sleep(Duration::from_millis(25));
        relay.activate(&marker_file, 77, "77-0").unwrap();
        writeln!(producer, "begin llm_request 5").unwrap();
        writeln!(producer, "instant llm_request 5 http_error").unwrap();
        writeln!(producer, "end llm_request 5").unwrap();
        writeln!(producer, "begin tool 1 /workspace/secret").unwrap();
        drop(producer);
        relay.finish();
        let text = std::fs::read_to_string(&marker).unwrap();
        assert!(text.contains("S|77|asb.llm.request.77-0|5"));
        assert!(text.contains("I|77|asb.llm.request.http_error.77-0"));
        assert!(text.contains("F|77|asb.llm.request.77-0|5"));
        assert!(!text.contains("/workspace/secret"));
        let _ = std::fs::remove_dir_all(root);
    }
}
