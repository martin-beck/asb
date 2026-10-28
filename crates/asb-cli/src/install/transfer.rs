//! Byte-accurate, terminal-safe progress for ASB-owned source transfers.
//!
//! Installers use external programs for transport and extraction so their
//! process/custody boundaries remain unchanged.  This module observes only the
//! private destination file and the response headers written beside it; it
//! never logs URLs, headers, command lines, or source bytes.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_millis(200);
const HEADER_LIMIT: usize = 64 * 1024;
pub(crate) const STATUS_ENV: &str = "ASB_TRANSFER_PROGRESS_FILE";

/// A bounded, machine-independent view of one byte-oriented operation.
///
/// `total_bytes` remains `None` when the remote endpoint does not disclose a
/// content length.  Callers must not invent a percentage in that case.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Progress {
    pub(crate) completed_bytes: u64,
    pub(crate) total_bytes: Option<u64>,
    pub(crate) bytes_per_second: u64,
    /// What the rate actually measures. Never present disk throughput as wire
    /// throughput when the package manager owns the HTTP client.
    pub(crate) rate_source: RateSource,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum RateSource {
    #[default]
    Network,
    DiskRead,
    DiskWrite,
}

impl Progress {
    pub(crate) fn percentage(self) -> Option<u8> {
        self.total_bytes
            .filter(|total| *total > 0)
            .map(|total| ((self.completed_bytes.saturating_mul(100) / total).min(100)) as u8)
    }
}

/// Observe an ASB-owned download destination while curl writes it.
///
/// The current snapshot is available to any owner through [`Download::snapshot`]. On
/// an interactive terminal this also owns one redrawable stderr line.  The
/// observer is presentation-only and cannot affect transfer success, source
/// validation, or process custody.
pub(crate) struct Download {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<Progress>>,
    worker: Option<JoinHandle<()>>,
}

/// Observe byte growth in an ASB-owned package cache.
///
/// Package managers such as npm and pip do not expose their individual HTTP
/// response bodies to ASB.  Their private cache is therefore the narrowest
/// reliable observation boundary available without changing the package
/// manager's invocation or consuming its output.  This measures cache-write
/// activity, not network bytes: a cache hit, decompression, or a package
/// manager that does not cache an artifact may legitimately report zero.
/// Totals remain unknown unless the package manager supplies one separately.
pub(crate) struct Cache {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<Progress>>,
    worker: Option<JoinHandle<()>>,
}

/// Progress for a local byte stream such as checksum validation.
///
/// Extraction adapters may use it when their archive reader exposes consumed
/// input bytes; callers leave the total unknown rather than manufacture a
/// percentage for formats that do not report it.
pub(crate) struct Activity {
    label: String,
    total: Option<u64>,
    started: Instant,
    last: Instant,
    completed: u64,
    interactive: bool,
    visible: bool,
}

impl Activity {
    pub(crate) fn new(label: impl Into<String>, total: Option<u64>) -> Self {
        let now = Instant::now();
        Self {
            label: label.into(),
            total,
            started: now,
            last: now.checked_sub(REFRESH).unwrap_or(now),
            completed: 0,
            interactive: io::stderr().is_terminal(),
            visible: false,
        }
    }

    pub(crate) fn update(&mut self, completed: u64) {
        self.completed = completed;
        let now = Instant::now();
        if now.duration_since(self.last) < REFRESH {
            return;
        }
        self.last = now;
        self.render(false);
    }

    pub(crate) fn finish(mut self) -> Progress {
        self.render(true);
        Progress {
            completed_bytes: self.completed,
            total_bytes: self.total,
            bytes_per_second: (self.completed as f64
                / Instant::now()
                    .duration_since(self.started)
                    .as_secs_f64()
                    .max(0.001)) as u64,
            rate_source: RateSource::DiskRead,
        }
    }

    fn render(&mut self, complete: bool) {
        if !self.interactive {
            return;
        }
        let elapsed = Instant::now()
            .duration_since(self.started)
            .as_secs_f64()
            .max(0.001);
        let progress = Progress {
            completed_bytes: self.completed,
            total_bytes: self.total,
            bytes_per_second: (self.completed as f64 / elapsed) as u64,
            rate_source: RateSource::DiskRead,
        };
        let mut stderr = io::stderr().lock();
        let result = if complete {
            writeln!(stderr, "\r\x1b[2K{}", line(&self.label, progress))
        } else {
            write!(stderr, "\r\x1b[2K{}", line(&self.label, progress))
        };
        if result.and_then(|()| stderr.flush()).is_ok() {
            self.visible = !complete;
        }
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        if self.visible {
            let mut stderr = io::stderr().lock();
            let _ = write!(stderr, "\r\x1b[2K");
            let _ = stderr.flush();
        }
    }
}

impl Download {
    pub(crate) fn start(destination: &Path, headers: &Path, label: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(Progress::default()));
        let worker = thread::Builder::new()
            .name("asb-download-progress".into())
            .spawn({
                let stop = Arc::clone(&stop);
                let state = Arc::clone(&state);
                let destination = destination.to_path_buf();
                let headers = headers.to_path_buf();
                let label = label.to_owned();
                move || observe_download(stop, state, destination, headers, label)
            })
            .ok();
        Self {
            stop,
            state,
            worker,
        }
    }

    pub(crate) fn snapshot(&self) -> Progress {
        *self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub(crate) fn finish(mut self) -> Progress {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let snapshot = self.snapshot();
        clear_snapshot();
        snapshot
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        clear_snapshot();
    }
}

impl Cache {
    pub(crate) fn start(root: &Path, label: &str) -> Self {
        Self::start_many(&[root], label)
    }

    /// Observe growth across disjoint ASB-owned cache or materialization roots.
    ///
    /// Package managers usually populate one cache directory, but some bounded
    /// catalog builders stream source bytes into a cache while simultaneously
    /// materializing normalized records elsewhere in the project.
    pub(crate) fn start_many(roots: &[&Path], label: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(Progress::default()));
        let worker = thread::Builder::new()
            .name("asb-package-cache-progress".into())
            .spawn({
                let stop = Arc::clone(&stop);
                let state = Arc::clone(&state);
                let roots = roots.iter().map(|root| (*root).to_path_buf()).collect();
                let label = label.to_owned();
                move || observe_cache(stop, state, roots, label)
            })
            .ok();
        Self {
            stop,
            state,
            worker,
        }
    }

    pub(crate) fn finish(mut self) -> Progress {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let snapshot = *self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        clear_snapshot();
        snapshot
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        clear_snapshot();
    }
}

/// Render a deterministic one-line transfer indicator without exposing a URL.
pub(crate) fn line(label: &str, progress: Progress) -> String {
    const WIDTH: usize = 16;
    let percent = progress.percentage();
    let filled = percent.map_or(0, |value| usize::from(value) * WIDTH / 100);
    let mut bar = String::with_capacity(WIDTH);
    for index in 0..WIDTH {
        bar.push(if index < filled { '=' } else { '-' });
    }
    let percent = percent.map_or_else(|| "?%".to_owned(), |value| format!("{value}%"));
    let offset = (WIDTH.saturating_sub(percent.len())) / 2;
    bar.replace_range(offset..offset + percent.len(), &percent);
    let total = progress.total_bytes.map_or_else(|| "?".into(), bytes);
    format!(
        "{label} [{bar}] {}/{} {}/s ({})",
        bytes(progress.completed_bytes),
        total,
        bytes(progress.bytes_per_second),
        match progress.rate_source {
            RateSource::Network => "network",
            RateSource::DiskRead => "disk read",
            RateSource::DiskWrite => "disk write",
        },
    )
}

fn observe_download(
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<Progress>>,
    destination: PathBuf,
    headers: PathBuf,
    label: String,
) {
    let interactive = io::stderr().is_terminal();
    let started = Instant::now();
    let mut previous_bytes = 0;
    let mut previous_at = started;
    let mut visible = false;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        let completed = fs::metadata(&destination)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let total = content_length(&headers);
        let elapsed = now.duration_since(previous_at);
        let speed = if elapsed.is_zero() {
            0
        } else {
            (completed.saturating_sub(previous_bytes) as f64 / elapsed.as_secs_f64().max(0.001))
                as u64
        };
        previous_bytes = completed;
        previous_at = now;
        let snapshot = Progress {
            completed_bytes: completed,
            total_bytes: total,
            bytes_per_second: speed,
            rate_source: RateSource::Network,
        };
        *state.lock().unwrap_or_else(|poison| poison.into_inner()) = snapshot;
        publish_snapshot(snapshot);
        if interactive {
            let mut stderr = io::stderr().lock();
            if write!(stderr, "\r\x1b[2K{}", line(&label, snapshot))
                .and_then(|()| stderr.flush())
                .is_ok()
            {
                visible = true;
            }
        }
        thread::sleep(REFRESH);
    }
    let now = Instant::now();
    let completed = fs::metadata(&destination)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let total = content_length(&headers);
    let seconds = now.duration_since(started).as_secs_f64().max(0.001);
    let snapshot = Progress {
        completed_bytes: completed,
        total_bytes: total,
        bytes_per_second: (completed as f64 / seconds) as u64,
        rate_source: RateSource::Network,
    };
    *state.lock().unwrap_or_else(|poison| poison.into_inner()) = snapshot;
    publish_snapshot(snapshot);
    if visible {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(stderr, "\r\x1b[2K{}", line(&label, snapshot));
        let _ = stderr.flush();
    }
}

fn observe_cache(
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<Progress>>,
    roots: Vec<PathBuf>,
    label: String,
) {
    let interactive = io::stderr().is_terminal();
    let started = Instant::now();
    let initial = roots.iter().fold(0_u64, |total, root| {
        total.saturating_add(directory_size(root))
    });
    let mut previous_bytes = 0;
    let mut previous_at = started;
    let mut visible = false;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        let completed = roots
            .iter()
            .fold(0_u64, |total, root| {
                total.saturating_add(directory_size(root))
            })
            .saturating_sub(initial);
        let elapsed = now.duration_since(previous_at);
        let speed = if elapsed.is_zero() {
            0
        } else {
            (completed.saturating_sub(previous_bytes) as f64 / elapsed.as_secs_f64().max(0.001))
                as u64
        };
        previous_bytes = completed;
        previous_at = now;
        let snapshot = Progress {
            completed_bytes: completed,
            total_bytes: None,
            bytes_per_second: speed,
            rate_source: RateSource::DiskWrite,
        };
        *state.lock().unwrap_or_else(|poison| poison.into_inner()) = snapshot;
        publish_snapshot(snapshot);
        if interactive {
            let mut stderr = io::stderr().lock();
            if write!(stderr, "\r\x1b[2K{}", line(&label, snapshot))
                .and_then(|()| stderr.flush())
                .is_ok()
            {
                visible = true;
            }
        }
        thread::sleep(REFRESH);
    }
    let completed = roots
        .iter()
        .fold(0_u64, |total, root| {
            total.saturating_add(directory_size(root))
        })
        .saturating_sub(initial);
    let seconds = Instant::now()
        .duration_since(started)
        .as_secs_f64()
        .max(0.001);
    let snapshot = Progress {
        completed_bytes: completed,
        total_bytes: None,
        bytes_per_second: (completed as f64 / seconds) as u64,
        rate_source: RateSource::DiskWrite,
    };
    *state.lock().unwrap_or_else(|poison| poison.into_inner()) = snapshot;
    publish_snapshot(snapshot);
    if visible {
        let mut stderr = io::stderr().lock();
        let _ = writeln!(stderr, "\r\x1b[2K{}", line(&label, snapshot));
        let _ = stderr.flush();
    }
}

/// Counts only regular files below an ASB-owned cache directory.  The cache
/// is intentionally not followed through symlinks, so a package manifest
/// cannot make progress observation walk arbitrary host data.
fn directory_size(root: &Path) -> u64 {
    const MAX_ENTRIES: usize = 16 * 1024;
    let mut total = 0;
    let mut seen = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let Ok(entries) = fs::read_dir(path) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return total;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file()
                && let Ok(metadata) = entry.metadata()
            {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

pub(crate) fn read_snapshot(path: &Path) -> Option<Progress> {
    let text = fs::read_to_string(path).ok()?;
    let mut values = text.split_ascii_whitespace();
    let completed_bytes = values.next()?.parse().ok()?;
    let total_bytes = match values.next()? {
        "?" => None,
        value => Some(value.parse().ok()?),
    };
    let bytes_per_second = values.next()?.parse().ok()?;
    let rate_source = match values.next()? {
        "network" => RateSource::Network,
        "disk-read" => RateSource::DiskRead,
        "disk-write" => RateSource::DiskWrite,
        _ => return None,
    };
    (values.next().is_none()).then_some(Progress {
        completed_bytes,
        total_bytes,
        bytes_per_second,
        rate_source,
    })
}

fn publish_snapshot(snapshot: Progress) {
    let Some(path) = std::env::var_os(STATUS_ENV).map(PathBuf::from) else {
        return;
    };
    let total = snapshot
        .total_bytes
        .map_or_else(|| "?".into(), |value| value.to_string());
    // The parent treats a partial write as unavailable. This is presentation
    // data only and never decides installation success.
    let _ = fs::write(
        path,
        format!(
            "{} {total} {} {}\n",
            snapshot.completed_bytes,
            snapshot.bytes_per_second,
            match snapshot.rate_source {
                RateSource::Network => "network",
                RateSource::DiskRead => "disk-read",
                RateSource::DiskWrite => "disk-write",
            },
        ),
    );
}

fn clear_snapshot() {
    if let Some(path) = std::env::var_os(STATUS_ENV).map(PathBuf::from) {
        let _ = fs::remove_file(path);
    }
}

fn content_length(headers: &Path) -> Option<u64> {
    let bytes = fs::read(headers).ok()?;
    let text = std::str::from_utf8(&bytes[..bytes.len().min(HEADER_LIMIT)]).ok()?;
    text.lines().rev().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<u64>().ok())
            .flatten()
    })
}

fn bytes(value: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit + 1 < UNITS.len() {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{amount:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_line_uses_known_or_unknown_total_without_guessing() {
        assert!(
            line(
                "Downloading fixture",
                Progress {
                    completed_bytes: 512,
                    total_bytes: Some(1024),
                    bytes_per_second: 256,
                    rate_source: RateSource::Network
                }
            )
            .contains("[======50%-------] 512 B/1.0 KiB 256 B/s")
        );
        assert!(
            line(
                "Downloading fixture",
                Progress {
                    completed_bytes: 512,
                    total_bytes: None,
                    bytes_per_second: 256,
                    rate_source: RateSource::Network
                }
            )
            .contains("[-------?%-------] 512 B/? 256 B/s")
        );
    }

    #[test]
    fn content_length_uses_the_final_redirect_response() {
        let path = std::env::temp_dir().join(format!(
            "asb-progress-{}",
            crate::tools::runtime::unique_id()
        ));
        fs::write(&path, "HTTP/1.1 302 Found\r\nContent-Length: 7\r\n\r\nHTTP/2 200\r\ncontent-length: 42\r\n\r\n").unwrap();
        assert_eq!(content_length(&path), Some(42));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn shared_snapshot_rejects_partial_or_extra_fields() {
        let path = std::env::temp_dir().join(format!(
            "asb-transfer-snapshot-{}",
            crate::tools::runtime::unique_id()
        ));
        fs::write(&path, "512 1024 256 disk-write\n").unwrap();
        assert_eq!(
            read_snapshot(&path),
            Some(Progress {
                completed_bytes: 512,
                total_bytes: Some(1024),
                bytes_per_second: 256,
                rate_source: RateSource::DiskWrite,
            })
        );
        fs::write(&path, "512 ?\n").unwrap();
        assert_eq!(read_snapshot(&path), None);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn directory_size_ignores_symlinks_and_counts_regular_files() {
        let root = std::env::temp_dir().join(format!(
            "asb-cache-progress-{}",
            crate::tools::runtime::unique_id()
        ));
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("one"), [0_u8; 7]).unwrap();
        fs::write(root.join("nested/two"), [0_u8; 11]).unwrap();
        assert_eq!(directory_size(&root), 18);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn package_cache_reports_byte_growth_without_inventing_a_total() {
        let root = std::env::temp_dir().join(format!(
            "asb-cache-progress-{}",
            crate::tools::runtime::unique_id()
        ));
        fs::create_dir_all(&root).unwrap();
        let monitor = Cache::start(&root, "Downloading package cache");
        // Give the observer one baseline tick before the package-manager
        // cache receives its simulated artifact.
        thread::sleep(REFRESH + REFRESH);
        fs::write(root.join("artifact"), [0_u8; 4096]).unwrap();
        thread::sleep(REFRESH + REFRESH);
        let progress = monitor.finish();
        assert_eq!(progress.completed_bytes, 4096);
        assert_eq!(progress.total_bytes, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn package_cache_reports_growth_from_every_owned_root() {
        let root = std::env::temp_dir().join(format!(
            "asb-cache-progress-many-{}",
            crate::tools::runtime::unique_id()
        ));
        let cache = root.join("cache");
        let output = root.join("output");
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&output).unwrap();
        let monitor = Cache::start_many(&[&cache, &output], "Materializing fixture catalog");
        thread::sleep(REFRESH + REFRESH);
        fs::write(cache.join("download"), [0_u8; 4096]).unwrap();
        fs::write(output.join("record"), [0_u8; 2048]).unwrap();
        thread::sleep(REFRESH + REFRESH);
        assert_eq!(monitor.finish().completed_bytes, 6144);
        fs::remove_dir_all(root).unwrap();
    }
}
